# savestate design

savestate runs QEMU/KVM virtual machines on Kubernetes, saves their complete state
(RAM, CPU and device state, disk) as named checkpoints, and starts any number of new
VMs from a checkpoint. It is a Kubernetes operator written in Rust that drives stock
QEMU over QMP. It shares its QEMU, QMP and guest-agent code with
[orbd](https://github.com/samcm/orbd).

Status: the engine runs: controller, node agent, launcher and CLI boot, checkpoint
and restore sandboxes end to end, export checkpoints to S3-compatible object stores
and restore them from there (phase 4), and expose guest ports to the cluster;
node-to-node copies come later. This document is the contract every part of the
implementation follows; change it in the same pull request as any change to the
behaviour it describes.

## User contract

- A **sandbox** is one Linux VM (for example 4 vCPU, 8 GiB RAM, 60 GiB disk) on one
  node, booted from a disk image or started from a checkpoint. It runs until it is
  deleted. You run commands in it and copy files in and out.
- A **checkpoint** is a named, read-only, complete picture of a sandbox at one
  instant: RAM, CPU and device state, and disk. Taking one pauses the sandbox while
  its RAM is written out, then it carries on. The pause grows with the RAM the guest
  is using, not counting its page cache, which is dropped first: 1–2 s for an idle
  8 GiB guest; for a busy one, about its used RAM divided by the node disk's write
  rate (18 s for 6.8 GiB at 400 MB/s, about 7 s at 1 GB/s). Before the pause the
  guest keeps running for a few seconds (at most 30) while the memory it freed goes
  back to the host. A checkpoint is the only kind of saved state.
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
| Network | A sandbox reaches the internet, and private addresses only where the installation allows. Nothing reaches it except the ports it exposes (`spec.ports`), from the sources the installation allows, and `port-forward` through the Kubernetes API. Every sandbox, forks included, is reachable on its own address and its own Service. |
| Memory | Every running sandbox costs its full memory size. Nothing shares or overcommits guest RAM; KSM stays off. |
| Placement | A sandbox from a checkpoint starts on a node that holds the checkpoint. One from a checkpoint in an object store can start on any node that can run it (holders are preferred): that node first copies the checkpoint from the store while the sandbox waits in `WaitingForTransfer`. |
| Durability | A checkpoint is taken on node-local disk. One held by no store is lost with its node, so that node is kept from cluster-autoscaler scale-down while it holds one. Export to an object store (per checkpoint, or for all by default) makes it independent of every node; node-local copies of exported checkpoints are then a cache the node may drop under disk pressure. |
| Autoscaling | The upstream cluster autoscaler adds nodes for pending VM pods and removes nodes without VMs and without the only copy of a checkpoint. A VM pool needs at least one node: from zero, an autoscaler whose node templates lack `savestate.dev/vm` adds none. See "Running on an autoscaled cluster" in `deploy/README.md`. |
| Lifetime | A checkpoint lives until its TTL expires or it is deleted, and cannot be deleted while a sandbox's disk still builds on it. Deleting it deletes its copies on nodes and in stores. |
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
  ports:                 # optional: guest ports reachable on the VM pod's address
    - {name: http, port: 8080}   # protocol: TCP (default) | UDP; name optional
  service: true          # optional: a ClusterIP Service named after the sandbox
status:
  phase: Running         # Pending | WaitingForTransfer | Booting | Restoring | Running | Failed
  node: node-a
  from: {checkpoint: web-1-parked}
  message: …             # why it is in this phase, mainly for Failed
---
kind: Checkpoint         # short name ckpt
metadata: {name: web-1-parked}
spec:
  sandbox: web-1
  ttl: 48h
  export: archive        # optional: store to export to once Ready (default: config `export`)
status:
  phase: Ready           # Pending | Saving | Ready | Failed
  nodes: [node-a]        # every node holding a complete copy
  stores: [archive]      # every store holding a complete copy
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
kind: Transfer           # short name xfer
metadata: {name: web-1-parked-to-archive}
spec:
  checkpoint: web-1-parked
  from: {node: node-a}   # export: from a node holding it to a store
  to: {store: archive}   #   or import: from: {store: archive}, to: {node: node-b}
status:
  phase: Copying         # Pending | Copying | Ready | Failed
  bytesDone: …           # bytes of the checkpoint's files copied or found already there
  bytesTotal: …
  message: …
```

- A sandbox has exactly one of `image` and `from`. An image is pinned by digest. A
  sandbox from an image sets `vcpus`, `memory` and `disk`; one started from a
  checkpoint inherits image, vCPUs, memory and disk from it and setting them is
  rejected (admission cannot see the checkpoint, so equal values are refused too).
  `clock` is only accepted with `from`; unset means `frozen`.
- Specs are immutable except `ttl`, a sandbox's `ports` and `service`, and a
  checkpoint's `export`: changing a sandbox's source, shape or clock, a checkpoint's
  sandbox, or anything in a transfer is rejected.
- `ports` exposes guest ports, by default none (see "Exposed ports"): each entry is
  a port (1–65535) and a protocol (`TCP`, the default, or `UDP`), at most once each,
  with an optional name (a DNS label, unique; the Service port's name, else
  `tcp-8080` style); at most 32. Exposure is host-side, not part of the VM, so a
  sandbox from a checkpoint inherits none and sets its own. Changing `ports` on a
  running sandbox takes effect within seconds. `service: true` needs at least one
  port and a sandbox name that is a DNS label starting with a letter.
- A TTL is whole numbers each followed by `d`, `h`, `m` or `s` (`7d`, `48h`,
  `1d12h`), counted from the object's creation. The installation config can set a
  default and a maximum per kind.
- The controller writes Sandbox status; the node agent doing the work writes
  Checkpoint and Transfer status (the controller marks them `Failed` when they cannot
  start). `Failed` is terminal for every kind.
- `kubectl get savestate` lists all three kinds. Field selectors work on
  `status.node` and `spec.from.checkpoint` (sandboxes), `spec.sandbox`
  (checkpoints), and `spec.checkpoint`, `spec.from.node` and `spec.to.node`
  (transfers), so node agents watch only their own node's objects.
- A `Transfer` copies a checkpoint between a node and a store (`stores` in the
  installation config): an export goes `from: {node}`, a node holding the checkpoint,
  `to: {store}`; an import goes `from: {store}`, one holding it, `to: {node}`. Each
  end names exactly one of `node` and `store`; node-to-node and store-to-store
  copies are refused (node-to-node comes later). The node agent of the transfer's
  node does the copy.
- A completed export adds its store to the checkpoint's `status.stores`, a completed
  import its node to `status.nodes`. The copy belongs to the checkpoint; deleting the
  `Transfer` afterwards only deletes the record. Deleting a `Transfer` that is still
  copying cancels it and removes partial files (an export's partial objects go with
  the store's garbage collection). Each node runs at most `transfers.perNode`
  transfers at a time (default one).
- A checkpoint with `spec.export` (else the config's `export`) gets an export
  `Transfer` from the controller once it is `Ready`, named `<checkpoint>-to-<store>`
  and owned by the checkpoint. A failed export is not retried; delete its `Transfer`
  to have the controller create it again.
- A sandbox from a checkpoint held by a store may be scheduled onto a node that does
  not hold the checkpoint; the controller then creates an import `Transfer` to that
  node, named after the sandbox and owned by it, and the sandbox waits in
  `WaitingForTransfer` until the node holds the checkpoint. A failed import fails the
  sandbox.
- Later, a `Migration` kind may move a running sandbox between nodes with QEMU live
  migration (for draining nodes). Not part of the MVP.

## CLI

`savestate` with a verb is shorthand over the objects above and, for `exec` and
`cp`, the guest agent. It keeps no state of its own: everything it shows is in the
objects, and everything it does can also be done with `kubectl`. It uses the current
kubeconfig context and namespace (`--namespace`, else `SAVESTATE_NAMESPACE`).

| Verb | What it does |
| --- | --- |
| `create NAME --image IMAGE --vcpus N --memory Q --disk Q` | creates a sandbox from an image and waits until it is `Running` |
| `create NAME --from CHECKPOINT [--clock stepped]` | creates a sandbox from a checkpoint and waits until it is `Running` |
| `create … --port PORT[/udp]… [--service]` | either of the above, with `spec.ports` and `spec.service` |
| `checkpoint SANDBOX [--name NAME] [--export STORE]` | waits until the sandbox runs, creates a checkpoint of it (named `<sandbox>-<random>` without `--name`) and waits until it is `Ready`, and with `--export` until the store holds it |
| `fork SANDBOX [-n N]` | one checkpoint as above, then `N` sandboxes from it named `<checkpoint>-1` to `<checkpoint>-N`, each with the forked sandbox's `ports` and `service`, and waits until they run |
| `list sandboxes\|checkpoints [-l SELECTOR]` | a table: name, phase, node and source of each sandbox; name, phase, sandbox, parent, nodes, stores and sizes of each checkpoint |
| `get NAME [--kind sandbox\|checkpoint]` | one sandbox (phase, node, source, shape, TTL, labels, message) or checkpoint (phase, sandbox, parent, nodes, stores, shape, sizes, guest time, TTL, labels, message) |
| `exec NAME [-i] [-e NAME=VALUE]… [--cwd DIR] -- CMD…` | runs a command in the guest with its stdin, stdout and stderr; exits with its exit code (128 + the signal if it was killed) |
| `exec NAME --detach [-e …] [--cwd DIR] -- CMD…` | starts the command in the background and prints its id |
| `ps NAME [ID]` | the detached processes: id, running or exit code (or signal), command |
| `logs NAME ID [-f]` | a detached process's stdout and stderr so far; with `-f` until it exits, then exits with its exit code |
| `cp SRC DST [--mode MODE]` | copies a file or a directory in or out; one side is `NAME:PATH` |
| `expose NAME PORT[/udp]… [--service]` | adds ports to the sandbox's `spec.ports` (and sets `spec.service`, then prints the Service's address) |
| `port-forward NAME LOCAL:GUEST…` | `kubectl port-forward` to the VM pod, which reaches any TCP port of the guest |
| `delete sandbox\|checkpoint NAME` | deletes the object |

- **Waiting.** `create`, `checkpoint` and `fork` wait as above, for at most
  `--timeout` in all (default `10m`). An object that becomes `Failed` ends the wait
  with a non-zero exit and its status message; a timeout says the phase and message
  the object was left in. `--no-wait` returns once the objects are created (a
  checkpoint still waits for its sandbox to run, since only a running sandbox can be
  checkpointed).
- **Labels.** `create`, `checkpoint` and `fork` take `--label KEY=VALUE`
  (repeatable). A checkpoint gets its sandbox's labels; a sandbox from a checkpoint
  (`create --from`, and the sandboxes of a fork) gets the checkpoint's labels, so a
  parked and resumed sandbox keeps its labels. `--label` adds to or overrides the
  inherited ones; a fork's `--label`s go on its new sandboxes. `list -l` takes a
  Kubernetes label selector (`app=web,tier!=test`).
- **Exec.** An attached command runs in the guest agent's cgroup with the agent's
  environment (`HOME=/root`) plus `--env`, in `--cwd` (default `/`); if the
  connection drops it keeps running with stdin closed, until it ends or the agent
  restarts. A detached command (`--detach`) runs as the transient systemd unit
  `savestate-exec-<id>.service`, outside the agent, so it outlives the exec, the
  launcher's relay and a restart of the agent, and goes on in sandboxes started from
  a checkpoint taken while it runs. Its stdin is `/dev/null`, its environment
  `HOME=/root` plus `--env`, and its stdout and stderr are kept in
  `/var/log/savestate/exec/<id>.stdout` and `.stderr`; systemd keeps the unit and its
  exit status after it exits, so `ps` and `logs` work until the sandbox is deleted.
  `systemctl stop savestate-exec-<id>` stops one.
- **Copies.** `cp` into a sandbox creates missing parent directories (mode 0755) and
  replaces a file atomically, with the source's permission bits or `--mode`; out of
  a sandbox it writes the guest's permission bits or `--mode`. A directory is copied
  as a tar stream: its contents go into the destination directory, created if
  missing, with their modes and modification times, owned by root in the guest and by
  the caller locally.
- **Output.** Text for people by default. `-o json` on `create`, `checkpoint`,
  `fork`, `list`, `get` and `ps` prints the objects instead (without `managedFields`), once
  any waiting is over: the object, an array for `list`, `{"checkpoint": …,
  "sandboxes": […]}` for `fork`, and for `ps` an array of `{"id", "command",
  "state": "running" | "exited", "exitCode" | "signal"}`.

## Architecture

```text
┌────────────────────┐   CRDs    ┌──────────────────────────┐
│ savestate          │◀─────────▶│ kube-apiserver           │
│ controller         │           └────────────▲─────────────┘
│ Sandbox → Pod,     │                        │ watch (own node only)
│ TTL, transfers,    │           ┌────────────┴─────────────┐
│ store GC           │           │ savestate node           │
└─────────┬──────────┘           │ (DaemonSet): layer store,│
          │ S3                   │ images, GC, transfers,   │
┌─────────▼──────────┐    S3     │ device plugin, seccomp   │
│ object stores      │◀──────────┤                          │
│ (off-cluster)      │           └────────────┬─────────────┘
└────────────────────┘             unix socket│
┌─────────────────────────────────────────────▼──────────────┐
│ VM pod (not privileged): savestate launcher → qemu (QMP)   │
│ no service-account token · memory request = limit ·        │
│ /dev/kvm, /dev/net/tun, /dev/vhost-vsock from the node     │
│ agent's device plugin · seccomp profile savestate/vm.json  │
│ tap + nftables NAT inside the pod's network namespace      │
│        └─ vsock ─▶ savestate-guest (exec, files, fixups)    │
└────────────────────────────────────────────────────────────┘
```

One static `savestate` binary provides every role (`controller`, `node`, `launcher`
and the CLI); `savestate-guest` runs inside the VM.

- **Controller** (Deployment, one replica): creates one Pod per Sandbox, in the
  sandbox's namespace, places sandboxes from checkpoints on nodes that hold them or
  can fetch them, enforces TTLs, creates Transfers, keeps finalizers that protect
  checkpoints still in use, and garbage-collects the object stores. VM pods run the
  launcher image from the installation config, by default the controller's own image
  at the digest its pod runs.
- **Node agent** (DaemonSet): owns `/var/lib/savestate` on its node; caches base images
  by digest; tells launchers on its node to boot, restore or checkpoint; garbage-collects
  layers, checkpoints and run directories whose objects are gone; exports checkpoints
  to object stores and imports them; drops exported copies under disk pressure; labels
  its node with its CPU identity and keeps it from scale-down while it holds the only
  copy of a checkpoint; reports disk use. As a kubelet device plugin it hands VM pods
  their devices, and it installs their seccomp profile. Only it holds API credentials
  on the node.
- **Launcher** (one per VM pod): starts and supervises QEMU, talks QMP, does the
  guest-agent work around boots, restores and checkpoints, streams the serial console
  to the pod log. It has no Kubernetes credentials; the node agent drives it over a
  unix socket (gRPC, `proto/savestate/launcher/v1/launcher.proto`). Exec, detached
  processes and file copy reach the guest through `savestate guest`, which the CLI runs in the VM pod with
  `pods/exec` and which talks to the guest agent over vsock.
- **Guest agent** (`savestate-guest`, inside the VM, built on orbd's guest library):
  exec (attached, or detached as transient systemd units), file and directory copy,
  the pre-checkpoint hook, and post-restore fixups (clock, entropy,
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
through RoleBindings; cluster-wide, the controller only reads nodes and the node
agent reads nodes and patches labels and annotations on them (its own; Kubernetes
RBAC cannot narrow that to one node). Both read the stores' credential Secrets in
their own namespace.

Capacity accounting is plain Kubernetes: each VM is a pod whose memory request and
limit are guest RAM plus the installation's memory overhead (`vm.memoryOverhead`,
default 512 MiB) and whose CPU request is its vCPUs unless the installation reserves
less (see "VM pods"), so the scheduler and ResourceQuota work unchanged. The spike
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
owned by it (deleting the sandbox deletes the pod), labelled
`app.kubernetes.io/name: savestate`, `app.kubernetes.io/component: vm` and
`savestate.dev/sandbox-uid: <sandbox UID>`, and annotated
`cluster-autoscaler.kubernetes.io/safe-to-evict: "false"` (evicting it destroys the
VM) and `savestate.dev/ports` (see "Exposed ports"):

- one container, `launcher`, running `savestate launcher` from the launcher image:
  the installation's (`launcherImage`, else the controller's own image by digest)
  for a sandbox from an image, the checkpoint's `fingerprint.launcherImage` for a
  sandbox from a checkpoint. The pod spec is the record of which image a sandbox
  runs; the controller never changes an existing pod except its
  `savestate.dev/ports` annotation (and replaces only an unscheduled one, see
  "Re-placing a pinned pod").
- resources: memory requests and limits are guest memory plus `vm.memoryOverhead`,
  never overcommitted; one `savestate.dev/vm`, the node agent's device plugin
  resource that brings `/dev/kvm`, `/dev/net/tun` and `/dev/vhost-vsock`; and the
  CPU request below. For a sandbox from a checkpoint, shape comes from the
  checkpoint's status.
- CPU: the request is the `cpuRequest` of the sandbox's shape (the `vm.shapes` entry
  with its vCPUs, memory and disk), else `vm.cpuRequestPerVcpu` per vCPU (default
  `1`). A request of a whole CPU per vCPU, the default, is also the CPU limit, so
  the pod is Guaranteed. A smaller request has no CPU limit, and the pod is
  Burstable (see "CPU requests below the vCPU count").
- `automountServiceAccountToken: false`, `enableServiceLinks: false`,
  `restartPolicy: Never`, `vm.nodeSelector`, `vm.tolerations`, and `vm.nodeAffinity` as
  the required node affinity, with the placement below added to every one of its terms. The container is
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
- for a sandbox from a checkpoint held by no store, required node affinity on
  `metadata.name` in the checkpoint's `status.nodes`. For one from a checkpoint in a
  store, required node affinity on the CPU label `savestate.dev/cpu` (see "CPU
  model") equal to the checkpoint's, and preferred affinity (weight 100) for the
  nodes in `status.nodes`: the scheduler picks a holder with room if there is one,
  else any node that can run the checkpoint, which then imports it.
- preferred pod affinity (weight 50, topology `kubernetes.io/hostname`) for nodes
  running other VM pods (`app.kubernetes.io/component: vm`, any namespace), so the
  scheduler packs VMs onto busy nodes and the cluster autoscaler can empty and remove
  the others. The scheduler normalises each plugin's score: the holder preference is
  the whole node-affinity score and the packing preference the whole pod-affinity
  score, and both plugins weigh the same by default, so packing can at most tie with
  the holder preference (where a holder runs no VM at all) and decides among the
  nodes that are not holders.
- host paths: `stateDir` at the same path, and the sandbox's run directory
  `/run/savestate/<namespace>/<sandbox>/<sandbox UID>/` at `/run/savestate`. The UID
  keeps a later sandbox of the same name from meeting an earlier one's files.
- a downward API volume at `/etc/savestate/pod` holding the pod's
  `savestate.dev/ports` annotation as the file `ports`.

**CPU requests below the vCPU count.** A 4-vCPU sandbox that requests 4 CPUs fits no
4-CPU node once the node reserves some CPU for itself, and only one on an 8-CPU
node. A shape may reserve less: its VM still has all its vCPUs, and they compete
for whatever CPU the node has free.

Such a pod has no CPU limit. A guest never runs more threads than its vCPUs, so a
limit equal to `vcpus` would only cap QEMU's own threads (I/O, and the migration
channels during a checkpoint) on top of them, and CFS throttling stops every vCPU of
the VM until the next period, which the guest sees as stalls. Without a limit a VM
uses at most its vCPUs plus QEMU's small overhead, taken from CPU nobody else is
using; under contention the kernel shares CPU in proportion to requests, so every
pod still gets at least its request. The cost is that such a VM's speed depends on
its neighbours: one requesting 500m for 4 vCPUs is promised half a CPU on a busy
node.

The pod is then Burstable rather than Guaranteed, which changes:

- Eviction under node memory pressure: the kubelet ranks pods by whether their
  memory use exceeds their request, then by priority, then by use above the request;
  QoS class is not part of it. A VM pod's memory request equals its limit, so its
  use never exceeds its request and it is evicted only after every pod that exceeds
  its own, exactly as when it was Guaranteed.
- The kernel's OOM killer, if the node runs out of memory before the kubelet
  evicts anything: Guaranteed containers get `oom_score_adj` −997 and are almost
  never chosen; Burstable ones get 1000 − 1000 × memory request / node memory
  (at least 2), so a VM becomes a candidate, larger VMs less so. A killed QEMU fails
  its sandbox.
- The kubelet's static CPU manager policy gives exclusive cores only to Guaranteed
  pods with whole CPUs; Burstable VM pods share the node's pool.

Memory is unaffected: request = limit = guest RAM plus overhead in every case.

Before creating the pod the controller refuses (sandbox `Failed`) an image outside
`images.registries`, a shape not in `vm.shapes`, and a checkpoint that is `Failed`,
being deleted, or `Ready` without fingerprint, vCPUs or memory. It waits (sandbox
`Pending` with a message) while the checkpoint does not exist, is not yet `Ready` or
is on no node and in no store. A pod of the same name owned by an earlier sandbox
is waited out.

**Re-placing a pinned pod.** A pod pinned to the holders of a checkpoint no store
had when it was created, which the scheduler finds no room for (`PodScheduled`
`False`, `Unschedulable`), is deleted once a store holds the checkpoint (with its UID
and resource version as preconditions, so a pod bound meanwhile stays), with a
`Replaced` event; the next pod may run on any node that runs the checkpoint's CPU,
including one the cluster autoscaler adds for it. An unscheduled pod has started
nothing, and the API server removes it at once. This is the only pod the controller
replaces.

**Imports.** When the VM pod of a sandbox from a checkpoint is scheduled onto a node
not in the checkpoint's `status.nodes`, the controller creates an import `Transfer`
named after the sandbox and owned by it, from the first store in `status.stores` to
that node (a transfer of the same name owned by an earlier sandbox is waited out).
The node agent there holds the launcher until the import is `Ready`, and fails it
with the transfer's message if the import fails.

**Exports.** For a `Ready` checkpoint whose export store (`spec.export`, else the
config's `export`) is not yet in `status.stores`, the controller creates the export
`Transfer` `<checkpoint>-to-<store>` (owned by the checkpoint) from the first node in
`status.nodes` that still exists. An export store missing from the config gets a
`Warning` event and no transfer.

**Exposed ports.** The controller keeps three things in step with a sandbox's
`ports` and `service`:

- the VM pod's `savestate.dev/ports` annotation: one `PORT/tcp` or `PORT/udp` per
  line, set when the pod is created and updated whenever `ports` changes. The
  launcher forwards exactly these (see "Network and security"). Container ports in
  the pod spec are not used: Kubernetes does not let them change on a running pod,
  and neither Services nor NetworkPolicies need them.
- while `ports` is not empty, a NetworkPolicy named after the sandbox and owned by
  it, selecting its VM pod by `savestate.dev/sandbox-uid` and allowing ingress on
  exactly those ports from `network.exposeFrom` (default: every pod in the sandbox's
  namespace). It adds to the namespace's policy refusing all ingress to VM pods.
- while `service` is true, a `ClusterIP` Service named after the sandbox and owned
  by it, selecting its VM pod by `savestate.dev/sandbox-uid`, with one port per entry
  in `ports` (same number, protocol and name).

Either object is deleted once `ports` is empty or `service` false. One of that name
that the sandbox does not own (an earlier sandbox's, or someone else's) is left
alone with a `Warning` event. Selecting by UID keeps a Service from reaching the
pod of an earlier sandbox of the same name. Each sandbox started from a checkpoint
has its own pod, so its exposure is its own: forks of one sandbox are reachable
independently, each at the same guest port behind its own address and Service.
External exposure (an Ingress or a `LoadBalancer` Service) is the installation's
choice; `deploy/README.md` has an example.

**Sandbox status** is derived from the pod (and, while it imports, its transfer):

| Pod | Sandbox |
| --- | --- |
| absent, not yet placed | `Pending` (message says what it waits for) |
| not scheduled | `Pending`, message from the scheduler's `PodScheduled` condition |
| on a node not holding its checkpoint | `WaitingForTransfer`; `node` set; message with the import's progress |
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
node agents remove the checkpoint's files and the store garbage collection its
objects.

**Store garbage collection.** Every minute, for each configured store, the
controller deletes what no checkpoint of its namespaces needs any more (see "Object
stores"). One store prefix belongs to one installation: objects under it that name
no checkpoint of this cluster are garbage.

**Contract with the node agent and launcher.**

| Who | Writes |
| --- | --- |
| Controller | VM pods (and their `savestate.dev/ports` annotation); Services and NetworkPolicies of sandboxes that expose ports; Sandbox `status` (all of it); TTL deletions; the Checkpoint finalizer; `Failed` on Checkpoints that cannot start; Transfers (exports and imports); deletions in stores |
| Node agent | Checkpoint `status` (`phase`, `nodes`, `stores`, `vcpus`, `memory`, `disk`, sizes, `guestTime`, `parent`, `fingerprint`, `message`); Transfer `status`; files under `stateDir`; objects in stores; its node's `savestate.dev/cpu` label and scale-down annotation; removes run directories of sandboxes that are gone |
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
A checkpoint not on this node, while this node is not in its `status.nodes` and a
store holds it, is waited for: the controller is importing it here. Starts that
cannot happen (an image that cannot be pulled, a checkpoint not on this node and in
no store, a failed import) are sent as `Fail`, and the launcher exits with that
message, so the sandbox fails with it. A booted guest gets its address, route, DNS and hostname from the
launcher through the guest agent; a restored one keeps what its checkpoint had and
gets the post-restore fixups.

**Base images.** A sandbox's `image` is a container image whose filesystem holds
`/rootfs.qcow2` (the `disk` stage of `guest/rootfs/Dockerfile`, or an installation's
equivalent). The node agent pulls it by digest (credentials from `images.auth`, plain
HTTP for `images.plainHttp`), checks the manifests against their digests, extracts the
disk, refuses one with a backing file, and publishes it read-only by rename. The
images in `images.prefetch` are pulled as soon as the node agent starts and are never
garbage; a sandbox booting from one while it is being pulled waits for that pull.

**Taking a checkpoint.** The node agent takes `Pending` checkpoints of sandboxes
`Running` on its node: it creates `checkpoints/<UID>.tmp/`, then sets the phase to
`Saving` with the checkpoint's resource version (so it never overrides the
controller's `Failed`), so every `Saving` checkpoint has a directory on its node. The
launcher runs the checkpoint steps below, writing the memory file and
`manifest.json` into that directory; the node agent publishes it by rename and writes
`Ready` with the manifest's shape, sizes, guest time, parent and fingerprint (the
launcher image is the VM pod's), and a `Saved` event with the pause. Before writing
`Ready` it marks its node as holding a checkpoint no store has (below). A save that fails
(including because its sandbox is deleted meanwhile) is discarded and `Failed`.
After a node agent restart, a `Saving` checkpoint with a directory on the node is
finished from disk: published, or complete (manifest written) and published now, is
`Ready`; anything else is discarded and `Failed`.

**Transfers.** The node agent runs the exports from its node (`spec.from.node`) and
the imports to it (`spec.to.node`), at most `transfers.perNode` at a time, writing
`Copying` with `bytesTotal`, then `bytesDone` every few seconds, then `Ready` or
`Failed` with a message and an event on the Transfer saying how many bytes moved and
how long it took. An export of a checkpoint not on the node, or to or from a store
not in the config, fails. A finished export adds the store to the checkpoint's
`status.stores`; an import downloads into `checkpoints/<UID>.tmp/` and
`<path>.part` files, publishes like a save, and adds the node to `status.nodes`
(both with the checkpoint's resource version, retried on conflict). Both resume
after a restart of the node agent: a `Copying` transfer is started again, and what
is already in the store, or already on the node, is not copied twice. A transfer
whose object is deleted while copying stops and removes its partial files.

**Scale-down.** While the node holds a `Ready` checkpoint whose `status.stores` is
empty, the node agent sets `cluster-autoscaler.kubernetes.io/scale-down-disabled:
"true"` on its node; once every local checkpoint is in a store or gone, it removes
the annotation. It recomputes this every minute and after each export, and sets it
before marking a new checkpoint `Ready`.

**Cache eviction.** A local copy of a checkpoint that a store holds is a cache.
When the state directory's filesystem has less than `cache.minFreePercent` free
(default 10), the node agent drops such copies, oldest checkpoint first, until it
has enough: it removes its node from `status.nodes` (with the checkpoint's resource
version), then the files; the next garbage collection removes the layers and images
nothing else uses. A copy that the sandbox of a VM pod on this node starts or runs
from is never dropped. A restore looks for its checkpoint, and an eviction lists the
VM pods and drops copies, under one lock, so a restore never starts from a copy
being dropped. Imports' files are kept from garbage collection until they are
published.

**Garbage collection.** Every minute the node agent lists its files, then the
Sandboxes and Checkpoints of its namespaces (fresh), and removes checkpoints (and
saves) whose object is gone, layers no existing sandbox owns and no remaining
checkpoint's chain (including a complete save's) uses, images no sandbox boots and no
chain uses, and run directories of sandboxes that are gone. Files are listed first, so
a file created meanwhile is never taken for garbage. It reports bytes per kind and
free bytes as metrics (`savestate_node_disk_bytes`, `savestate_node_disk_free_bytes`)
on `--metrics-listen` (default `0.0.0.0:9090`).

**Joining a node.** Before the node agent starts, its pod's init containers load
`vhost_vsock` where `/dev/vhost-vsock` is missing (`load-modules`, privileged, with
the host's `/lib/modules`) and check the CPU model (`probe-cpu`, below). The node
agent then installs the seccomp profile and only then advertises VM slots, so no VM
pod is admitted to a node before both its module and its profile are there. Image
prefetch runs alongside and is not waited for: a node takes VMs from checkpoints
(which import their image with the rest of the chain if it is missing) and VMs from
other images at once, and only a VM booting from an image still being pulled waits
for it. An installation may give new VM nodes the startup taint
`startup-taint.cluster-autoscaler.kubernetes.io/savestate` (`NoSchedule`; the node
agent tolerates it): the cluster autoscaler counts a node with that taint as still
starting, rather than as a node the pending VM pods cannot use, and so adds no
second node for them while the first comes up. The node agent removes the taint once
its node's allocatable shows `savestate.dev/vm`, and again within 30 s whenever it
reappears (a platform may copy an edited pool's taints onto its nodes).

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
records that guests get the node's CPU. At start-up the node agent labels its node
`savestate.dev/cpu` with the first 16 hex digits of the SHA-256 of the CPU guests
get there: the node's CPU (`hostCpu`, model name and flags digest) with `host`,
else the model name. A checkpoint's CPU label is the same digest of its
fingerprint's `hostCpu`, else its `cpuModel`, so a sandbox from a checkpoint in a
store is only scheduled onto nodes whose guests get exactly the CPU it was taken
with.

The controller serves Prometheus metrics (`--metrics-listen`, default
`0.0.0.0:9090`, at `/metrics`): reconcile counts, errors and durations per kind,
sandbox phase transitions, TTL deletions and store garbage collection. Logs are JSON lines from `tracing`.

## QEMU mechanics

Pinned by the launcher image: stock QEMU 11.1 on Debian 13 (built from the upstream
release until Debian 13 backports ships 11.1 for amd64) and an explicit
versioned machine type (`pc-q35-11.1`). The guest CPU model is installation
configuration (`vm.cpuModel`), one of:

- `host`: guests get the node's own CPU. The checkpoint's fingerprint records the
  node's CPU (`hostCpu`: model name and a digest of its feature flags), and a
  restore on a node whose CPU differs is refused; a checkpoint from a store is only
  placed on nodes with an identical CPU (`savestate.dev/cpu`). The simple choice
  where all VM nodes are one machine type.
- a named model that every node in the cluster can run (for example `EPYC-Rome-v4`
  where Zen 2 and Zen 3 hosts mix), without `invtsc`: what lets checkpoints move
  between any VM nodes. The launcher passes `-cpu <model>,enforce`, so a host that
  lacks a feature refuses to start rather than silently weakening the model; the
  node agent probes the configured model that way at start-up and reports a node
  that cannot run it. (Nested hosts often hide speculation
control features: on nested Zen 3 and Zen 4 hosts `EPYC-Rome-v4` is refused.) The
launcher image digest and the CPU model are part of every checkpoint's fingerprint,
and restores run that exact image. Measured numbers are in `spike/RESULTS.md`.

Checkpoint (sandbox keeps running):

1. Guest agent pre-checkpoint hook: `sync`, drop the page cache, then compact memory
   (`/proc/sys/vm/compact_memory`), so the freed memory forms the 2 MiB blocks the
   balloon's free page reporting hands back to the host; without compaction most of
   a dropped cache lies in smaller holes between used pages and is saved anyway.
   The guest kernel reports in passes 2 s apart, each a part of what is free, so the
   launcher then waits until QEMU's resident memory has not fallen by 16 MiB for
   2.5 s, at most 30 s. The guest runs meanwhile; only the RAM file shrinks. In the
   end-to-end test (1 GiB guest using 179 MiB plus 256 MiB of page cache) the RAM
   file went from 439 MiB to 186 MiB and the pause from 1.78 s to 0.70 s; the wait
   was 6.8 s. On a devnet guest with a large page cache the file went from 5.75 GiB
   to 2.87 GiB; where the RAM is genuinely in use it changes little.
2. `stop`.
3. `blockdev-snapshot`: a new overlay becomes the sandbox's active disk; the previous
   top layer is now immutable and belongs to the checkpoint. The overlay is created
   with `qemu-img create -f qcow2 -u -b <top layer> -F qcow2 <new> <size>` (`-u` and
   an explicit size, so `qemu-img` does not open the layer the running QEMU has open)
   and added with `blockdev-add` (`"backing": null`) before `blockdev-snapshot`.
4. Migrate to a file with `multifd` + `mapped-ram` (with `direct-io` only if
   `vm.memoryFileDirectIo`: on cloud VMs' local disks a save through the page cache
   wrote about 700 MB/s against 250 MB/s with O_DIRECT, pausing devnet guests 5.8 s
   instead of 12.0 s (2.9 GiB file) and 8.1 s instead of 26.7 s (5.5 GiB), and
   concurrent restores of one checkpoint share the cached file; on other disks
   O_DIRECT can be faster), with
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
  checkpoints/<UID>.tmp/             save or import in progress; complete once
                                     manifest.json exists
  images/*.part, layers/*.part       files being imported
  trash/                             delete-by-rename
```

Checkpoint directories are named by the Checkpoint's UID. A layer names its backing
file relative to its own directory (`<id>.qcow2`, `../images/<hex>.qcow2`), and a
manifest lists its chain relative to the state directory. Every publication is
build-then-rename and fsynced; the node agent reconciles what is on disk at start-up
(a complete save is published, an incomplete one discarded, interrupted image pulls
and imports removed), so a crash at any point converges (the orbd pattern). The node
agent owns the directory; `layers/` and each save's `.tmp` directory belong to the
QEMU user, who writes them. Chains grow by one layer per checkpoint; capping their
depth and flattening them in the background is phase 3.

## Object stores

An installation names S3-compatible object stores in its config (`stores`): an
endpoint URL (`https://…`, or `http://…` inside a trusted network), a bucket, a
region, a key prefix and the name of a Secret, in the namespace the controller and
node agents run in, holding `accessKeyId` and `secretAccessKey` (and optionally
`sessionToken`). The Secret is read for every transfer and garbage collection, so
rotating it needs no restart. Requests are path-style. The client is the
`savestate-objstore` crate, built on `object_store`'s S3 client.

Layout under `<prefix>/`:

```text
files/<path>/<sha256>          a chunk of the file's data, at most 16 MiB, named by
                               its digest
files/<path>/index.json        the file's size, data extents and chunk digests;
                               written after its chunks: the file is complete
exports/<UID>.json             an export of checkpoint <UID> in progress: the files
                               it references
manifests/<UID>.json           the checkpoint's manifest.json and its files; written
                               last: the checkpoint is complete
collections/<pass>.json        a garbage collection that may be deleting: written
                               before it lists anything, removed when it is done
```

`<path>` is the file's path in the state directory: `images/<hex>.qcow2`,
`layers/<id>.qcow2` and `checkpoints/<UID>/memory`. Each of those names one
immutable file (an image by digest, a layer once frozen, a memory file by its
checkpoint), so a file is uploaded once and every checkpoint whose chain uses it
shares it: exporting a child of an exported checkpoint uploads only its own layer and
memory file.

A file's data is its allocated extents (`SEEK_DATA`/`SEEK_HOLE`) in order: the
memory file holds only the guest's non-zero pages, so its holes cost nothing to
store or move. An import writes the extents back at their offsets into a file of the
original size, which is then as sparse as the original. Nothing is compressed: the
zero pages are already gone, and decompression would sit between the store and the
disk on every restore. Chunks move with 16 requests in flight per transfer (one
transfer buffers at most 256 MiB); each chunk is one PUT, so an interrupted export
leaves only whole chunks, and a retry skips the chunks and files already there.
Every chunk is checked against its digest when it is downloaded, before it is
written.

**Export** (node agent on a node holding the checkpoint): write
`exports/<UID>.json` listing the checkpoint's chain and memory file; wait until the
collections listed in `collections/` now have finished (at most 30 s); for each file, the memory file and newest layer first, skip it if its index exists, else
upload the chunks not yet there and then its index; write `manifests/<UID>.json`;
delete the export record. A checkpoint whose manifest exists is exported already.

**Import** (node agent of the destination node): read the manifest; for each file
not already on the node, download its chunks into `<path>.part` (the memory file
into `checkpoints/<UID>.tmp/`), fsync and rename it into place; write the
checkpoint's `manifest.json` into the `.tmp` directory last and publish it like a
save.

**Garbage collection** (controller, every minute per store): write its marker
`collections/<pass>.json`; list `files/`; then,
starting a 10 s deadline, list `exports/` and `manifests/`, then the Checkpoints of
its namespaces (fresh); delete the manifests and export records of checkpoints that
are gone; then delete every file, its index first, that no remaining manifest or
export record names, stopping at the deadline (the next pass carries on). Files are
listed before the records that name them, so a file uploaded meanwhile is never
taken for garbage, and records are listed before the Checkpoints, so a checkpoint
created meanwhile is never taken for gone. Its marker is removed once it is done.
An export trusts that a file or chunk is there only once the collections whose
markers it listed after writing its export record have finished: a collection whose
marker it did not see wrote it, and so listed the records, after the export's record
existed. Exports therefore wait only while a collection runs (well under a second
for most). A collection that fails leaves its marker; an export then waits at most
30 s after writing its record, by when any collection that listed the records
before it has passed its deadline and its last request's 5 s timeout, and the next
collection removes markers written more than 10 min before its own (by the store's
clock). One prefix belongs to one installation: GC deletes whatever names no
checkpoint of its own cluster.

## Network and security

- Each VM pod is its own network namespace: QEMU attaches a tap device (`tap0`,
  `10.0.2.2/24`); nftables in the pod (table `inet savestate`) masquerades guest
  traffic, drops traffic to private, shared, loopback, link-local, multicast and
  reserved ranges and to the node's own addresses, except `network.egressAllow` and
  port 53 of the guest's DNS servers, drops IPv6, and lets nothing reach the guest or
  the pod from it unsolicited except through its exposed ports. Every sandbox sees
  the same guest address (`10.0.2.15/24` via `10.0.2.2`, MAC `52:54:00:12:34:56`);
  DNS is `network.dns`, else the VM pod's own resolvers. Forwarding is enabled per
  interface through netlink.
- Exposed ports: the table holds a set `exposed` of protocol and port pairs.
  Connections arriving on the pod's own addresses from outside (not from the guest)
  to a pair in the set are DNATed to the guest at the same port, and the forward
  chain lets in exactly those connections (`ct status dnat`) and lets out the
  guest's replies to them (`ct direction reply`); everything the guest starts itself
  still meets the egress rules. Clients' addresses reach the guest unchanged. The
  set is replaced in one nftables transaction whenever the pod's
  `savestate.dev/ports` annotation changes (read from the downward API volume every
  second), by a process the launcher starts as root before it drops its privileges
  (`savestate launcher-ports`), which does nothing else. Nothing else inbound opens.
- Port-forward: `kubectl port-forward` connects to `127.0.0.1` inside the pod. TCP
  connections to `127.0.0.1` in the pod are DNATed to the guest on any port and
  masqueraded to `10.0.2.2` on the way in (`route_localnet` is set on the tap device
  so they may leave loopback). Only processes in the pod can make them, and the
  Kubernetes API authorizes port-forward (`pods/portforward`) as it does exec, so
  this needs no exposure; NetworkPolicies do not apply to it.
- The launcher builds that network as root, hands its run directory to the QEMU user
  (uid and gid 65532), starts `savestate launcher-ports`, and then drops to that user
  for good, keeping only the groups that own `/dev/kvm`, `/dev/net/tun` and
  `/dev/vhost-vsock`. QEMU runs as that user with `-sandbox on`; the launcher has no
  service-account token. Its container is never privileged: devices come from the
  node agent's device plugin, and the container keeps only `NET_ADMIN` (the tap
  device, forwarding and nftables in its own network namespace), `CHOWN` (handing the
  run directory to the QEMU user), `SETUID` and `SETGID` (becoming it), under
  savestate's seccomp profile.
- Exec, file copy and port-forward use Kubernetes `pods/exec` and `pods/portforward`
  RBAC; there is no separate API server. `savestate exec`, `ps`, `logs` and `cp` run
  `savestate guest` in the VM pod, which reaches the guest agent over vsock.
- VM pods need `NET_ADMIN` in their own network namespace and `hostPath` volumes
  (the node's state directory and their run directory). The baseline Pod Security
  level forbids both (a Localhost seccomp profile and device-plugin devices would
  pass it), so sandbox namespaces need the privileged level. They therefore hold
  only sandboxes; users get the `savestate-user` role (savestate objects, read pods,
  `pods/exec`, `pods/portforward`) and never pod creation there. A NetworkPolicy
  refuses all ingress to VM pods (`app.kubernetes.io/component: vm`); the
  controller's per-sandbox policies open exposed ports only (see "Exposed ports").
  Exec and port-forward go through the kubelet and the node agent uses a unix
  socket.

## Configuration boundary

The repository holds behaviour, schemas, defaults that are right for most clusters,
and examples with placeholder values. Anything that describes one particular
installation is supplied at install time and never committed here:

| In the repository | Supplied by each installation |
| --- | --- |
| CRD schemas, controller and node behaviour | Namespaces, RBAC subjects, quotas |
| Generic defaults (state directory, transfer concurrency, cache eviction threshold, TTL limits) and their config schema | Node selection, tolerations, which nodes run VMs |
| Install manifests with placeholders (kustomize base, example values) | Guest CPU model, VM shapes offered and the CPU they request, memory overhead margin |
| Guest image build recipe (kernel config, minimal rootfs, guest agent) | Base images and what is preinstalled in them, registries, pull secrets |
| Test fixtures | Egress allowlists, DNS, who may reach exposed ports, object stores (endpoint, bucket, region, prefix), which store checkpoints export to, credentials |

Installation config is one YAML file (operator and node agent) plus the usual
Kubernetes objects; every field has a documented default or is required with a clear
error, and unknown fields are refused. The schema is `savestate_api::config::Config`:
`namespaces` (required), `stateDir`, `kubeletDir`, `launcherImage`, `vm` (`cpuModel`
required, `memoryOverhead`, `cpuRequestPerVcpu`, `shapes` with an optional
`cpuRequest` each, `nodeSelector`, `nodeAffinity`, `tolerations`, `slotsPerNode`, `memoryFileDirectIo`), `network`
(`egressAllow`, `dns`, `exposeFrom`), `images` (`registries`, `auth`, `plainHttp`, `prefetch`), `stores` (per name:
`endpoint`, `bucket`, `region` and `credentialsSecret` required, `prefix`), `export`
(a store name; default none), `cache.minFreePercent`, `transfers.perNode` and `ttl`
(`default` and `max` per kind). `deploy/base` is a kustomize base with
placeholder values and `deploy/README.md` lists what an installation supplies.

## Phases

| Phase | Scope |
| --- | --- |
| 0. Spike | Measure and prove the QEMU mechanics above on real KVM hosts: save/restore timings for an 8 GiB guest, parent `cont` after checkpoint, three concurrent restores from one RAM file and one frozen layer (image locking), frozen and stepped clocks, VM generation ID change, MAC behaviour, baseline CPU model on Zen 2 and Zen 3, cross-CPU restore. |
| 1. Engine | Shared crates with orbd; launcher, layer store, checkpoint and restore flows, guest agent fixups, CLI creating VM pods directly. |
| 2. Operator | CRDs, controller, node agent, GC and retention, metrics, install manifests. |
| 3. Hardening | Failure injection, disk-full, node reboot, upgrades, soak. |
| 4. Transfers | `Transfer` kind; export to S3-compatible object stores and restore from them onto any node that can run the checkpoint; cache eviction and scale-down protection on autoscaled clusters. |
| Later | Node-to-node copies, `Migration`, lazy restore, optional KSM. |
