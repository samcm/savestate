# Guest kernel and base image

What runs inside every savestate VM: a kernel QEMU boots directly and a disk whose
root filesystem carries systemd and the guest agent (`crates/savestate-guest`).

- `kernel/`: `build.sh` builds the guest kernel from the latest stable Linux series
  (everything built in, no modules, no initrd), configured with orbd's guest config
  fragment, at the orbd revision `Cargo.lock` pins, as the starting point and
  `savestate.config` on top: VM generation ID, virtio-vsock, virtio-blk and virtio-net,
  kvmclock, free page reporting and the serial console.
- `rootfs/`: the generic base image, a Dockerfile that `build.sh` turns into
  `rootfs.qcow2`: Debian 13, systemd, udev, D-Bus, `iproute2`, `procps` and the guest
  agent, nothing else. It is a base to build on, not something to run work in as is.
  Every push to main publishes it, `:main` and `:sha-<short>`, as the sandbox image
  `ghcr.io/samcm/savestate-base` (its disk) and as the container image
  `ghcr.io/samcm/savestate-rootfs` (the `base` stage, to build FROM).

## Build

```sh
guest/kernel/build.sh                                     # target/guest-kernel/bzImage
cargo build --release --target x86_64-unknown-linux-musl -p savestate-guest
guest/rootfs/build.sh disk \
    target/x86_64-unknown-linux-musl/release/savestate-guest target/guest-rootfs
                                                          # target/guest-rootfs/rootfs.qcow2
```

The kernel build needs `gcc make flex bison bc libelf-dev libssl-dev`; the image build
needs Docker with buildx. `SIZE` (default `2G`) sets the image's filesystem size.

## Building an installation's image on top

The base holds no software, users, network or DNS configuration. Each installation
builds its own image from it, as a container image, then converts that to a disk:

```dockerfile
# example/Dockerfile
FROM savestate-base
RUN apt-get update && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/*
COPY after-restore.d/ /etc/savestate/after-restore.d/
```

`savestate-base` is the `base` stage: the published `ghcr.io/samcm/savestate-rootfs`
(pin it by digest), or built locally:

```sh
guest/rootfs/build.sh base target/x86_64-unknown-linux-musl/release/savestate-guest savestate-base
docker build -t ghcr.io/example/base-rootfs example/
SIZE=8G guest/rootfs/build.sh disk-from ghcr.io/example/base-rootfs out/   # out/rootfs.qcow2
```

Keep these from the base:

- No NTP client (`systemd-timesyncd` is masked; do not add chrony or ntpsec): savestate
  owns guest time, frozen or stepped after a restore.
- `savestate-guest.service` enabled, `/etc/machine-id` empty, `/etc/fstab` as shipped.
- Programs that must react to a restore (new hostname, machine id, clock) go in
  `/etc/savestate/after-restore.d/`.
- SSH host keys generated while building the image (as installing `openssh-server`
  does) are shared by every sandbox booted fresh from it; restores replace them. Remove
  them from the image and generate them at first boot if fresh sandboxes need their own.

## Host contract

QEMU boots the kernel with `-kernel bzImage -append "console=ttyS0 root=/dev/vda rw net.ifnames=0"`
(add `systemd.hostname=<name>` to name a fresh sandbox) and these guest devices:

| Device | Why |
| --- | --- |
| virtio-blk, the first disk (`/dev/vda`) | The root filesystem: one ext4 filesystem, no partition table. A qcow2 overlay larger than the image is grown into at boot (`x-systemd.growfs`). |
| `vhost-vsock-pci,guest-cid=<cid>` | The guest agent, on vsock port 1024. Context ids are global on the host; a restored copy gets its own. |
| `vmgenid,guid=auto` | A new generation id on every restore, so the guest kernel reseeds its RNG. |
| serial port (ttyS0) | Kernel and agent log; a login prompt. |
| `-rtc clock=vm` | The RTC follows the guest clock, which a restore leaves frozen. |
| `virtio-balloon-pci,free-page-reporting=on` (recommended) | Memory the guest frees, such as the page cache the pre-checkpoint hook drops, is returned to the host and left out of the memory file. |

### Guest agent

`savestate-guest` serves `savestate.guest.v1`
([`proto/savestate/guest/v1/guest.proto`](../proto/savestate/guest/v1/guest.proto)) on
vsock port 1024; the host side is `savestate_guest_proto::Client`. One operation per
connection:

- `Hello`: agent version and boot id (unchanged by a restore).
- `Exec`: run a program as root with streamed stdin, stdout and stderr; returns its
  exit code or signal.
- `ReadFile`, `WriteFile`: copy one file out or in (written atomically).
- `PrepareCheckpoint`: sync and drop the page cache before the host stops the VM.
- `Restore`: after a restored VM runs again, in this order: step the wall clock to a
  host-supplied time (clock `stepped` only), credit host-supplied entropy to the
  kernel, write a new machine id, set the hostname, regenerate the SSH host keys present
  in `/etc/ssh`, then run `/etc/savestate/after-restore.d/*` with `SAVESTATE_SANDBOX`,
  `SAVESTATE_CHECKPOINT` and `SAVESTATE_CLOCK` set.
