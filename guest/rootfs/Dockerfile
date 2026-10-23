# syntax=docker/dockerfile:1
#
# savestate's generic guest root filesystem, built as a container image and converted
# into a bootable qcow2 disk (one ext4 filesystem, no partition table, booted with
# `root=/dev/vda` by the kernel from guest/kernel). Build it with guest/rootfs/build.sh,
# which passes the static `savestate-guest` binary as the build context `agent`.
#
# Stages:
#   base     Debian 13 with systemd, udev and the guest agent; nothing else.
#   rootfs   the filesystem that becomes the disk. It is `base`, unless the build
#            replaces it with an installation's own image built FROM base
#            (`--build-context rootfs=docker-image://...`, see guest/README.md).
#   disk     the qcow2 (`rootfs.qcow2`), for `--output`.

FROM debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad AS base
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
# No NTP client: savestate owns guest time (frozen or stepped clocks after a restore).
RUN apt-get update \
    && apt-get install -y --no-install-recommends systemd systemd-sysv udev dbus iproute2 procps \
    && rm -rf /var/lib/apt/lists/*
COPY --from=agent --chmod=0755 savestate-guest /usr/local/sbin/savestate-guest
COPY etc/ /etc/
# - An empty machine-id: systemd generates one on the first boot of each disk.
# - The serial console (ttyS0) gets a login prompt through systemd's getty generator.
RUN systemctl enable savestate-guest.service \
    && systemctl mask systemd-timesyncd.service \
    && mkdir -p /etc/savestate/after-restore.d \
    && : >/etc/machine-id \
    && ln -sf /etc/machine-id /var/lib/dbus/machine-id

FROM base AS rootfs

FROM debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad AS convert
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
RUN apt-get update \
    && apt-get install -y --no-install-recommends e2fsprogs qemu-utils \
    && rm -rf /var/lib/apt/lists/*
# Size of the root filesystem in the image. A sandbox's disk is usually larger; the
# guest grows the filesystem to fill it at boot (x-systemd.growfs in /etc/fstab).
ARG SIZE=2G
RUN --mount=type=bind,from=rootfs,target=/rootfs \
    test -x /rootfs/usr/local/sbin/savestate-guest \
    && mkfs.ext4 -q -L root -d /rootfs /disk.raw "$SIZE" \
    && qemu-img convert -f raw -O qcow2 /disk.raw /rootfs.qcow2 \
    && rm /disk.raw

FROM scratch AS disk
COPY --from=convert /rootfs.qcow2 /
