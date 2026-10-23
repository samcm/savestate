<p align="center">
  <img src="assets/logo.svg" width="96" height="96" alt="savestate logo">
</p>

<h1 align="center">savestate</h1>

Save a running VM, then start as many copies of that exact moment as you like, on
Kubernetes.

> **Early.** Sandboxes boot, checkpoint, restore and fork on one node; copying
> checkpoints between nodes (transfers) is not built yet. The design is in
> [docs/design.md](docs/design.md), which is the detailed reference; installing is in
> [deploy/README.md](deploy/README.md).

## Why

Some work is expensive to reach and cheap to repeat from: a booted system with its
services warmed up, a build half done, a test environment in an interesting state.
Containers can save files but not running processes, so every new copy starts from
scratch.

savestate runs each sandbox as a QEMU/KVM virtual machine and saves its complete
state (RAM, CPU and device state, disk) as a named checkpoint. Any number of new
sandboxes start from a checkpoint with the same processes mid-flight, each fully
independent. Parking a sandbox and branching it are the same operation, and
capacity, scheduling and quotas stay plain Kubernetes.

## How it works

A controller turns each `Sandbox` object into one Guaranteed VM pod. A node agent
on every node owns that node's base images, disk layers and checkpoints, and tells
the launcher in each VM pod to checkpoint or restore. The launcher drives stock QEMU
over QMP; a guest agent inside the VM runs commands, copies files and fixes up clock,
entropy and identity after a restore. Controller, node agent, launcher and CLI are
the same static `savestate` binary.

```
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
│ no service-account token · /dev/kvm, /dev/net/tun,         │
│ /dev/vhost-vsock                                           │
│ tap + nftables NAT inside the pod's network namespace      │
│        └─ vsock ─▶ savestate-guest (exec, files, fixups)    │
└────────────────────────────────────────────────────────────┘
```

## What you get

- A **sandbox** is one Linux VM on one node, booted from a disk image or started from
  a checkpoint. It runs until you delete it; you run commands in it and copy files in
  and out.
- A **checkpoint** is a named, read-only picture of a sandbox at one instant. Taking
  one pauses the sandbox for a few seconds; it then carries on.
- **Starting from a checkpoint** wakes exactly where the checkpoint was: same
  processes, same files, same in-guest address. The guest's clock is frozen by
  default (it believes no time passed) or stepped to the host's time; every copy gets
  fresh entropy, machine-id, hostname and SSH host keys.
- Checkpoints live on node-local disk; a sandbox from one starts on a node that holds
  it. Every running sandbox costs its full memory size.

```sh
savestate create web-1 --image ghcr.io/example/base@sha256:…
savestate exec web-1 -- make build
savestate fork web-1 -n 3        # one checkpoint, three new sandboxes
savestate checkpoint web-1 --name web-1-parked
savestate delete sandbox web-1
savestate create web-1 --from web-1-parked
```

## Build

```sh
cargo build --release -p savestate
```

The guest kernel, the generic guest base image and how an installation builds its own
image on top are in [guest/README.md](guest/README.md).

The image `ghcr.io/samcm/savestate` (stock QEMU 11.1 on Debian 13) is published
from `main`; static binaries for x86_64 and aarch64 are attached to each release.

## License

[MIT](LICENSE)
