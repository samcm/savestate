# syntax=docker/dockerfile:1
#
# ghcr.io/samcm/savestate: every role of the `savestate` binary. linux/amd64 also
# carries QEMU for the launcher; linux/arm64 runs the controller and the CLI. Built
# from prebuilt artifacts (the build.yml outputs) in the context:
#
#   amd64/savestate  arm64/savestate
#
#   docker buildx build --platform linux/amd64,linux/arm64 -f Dockerfile <context>

# QEMU 11.1, stock and unpatched, built from the upstream release for x86_64 guests
# only. The machine type (pc-q35-11.1) and every checkpoint's fingerprint depend on
# this build, and a checkpoint restores only on the image it was taken with.
# Temporary: trixie-backports carries 11.1 but its amd64 build currently fails, so the
# amd64 archive still has 11.0. Once it publishes 11.1 for amd64, install
# `-t trixie-backports qemu-system-x86 qemu-utils` instead and drop this stage.
FROM debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad AS qemu
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
ARG QEMU_VERSION=11.1.2
ARG QEMU_SHA256=731b5681e4bb18be313231579b8efd0296c5b015fa36dc533874b639ba838016
RUN apt-get update && apt-get install -y --no-install-recommends \
        bison bzip2 ca-certificates curl flex gcc libaio-dev libc6-dev libglib2.0-dev \
        libpixman-1-dev libseccomp-dev liburing-dev libzstd-dev make ninja-build pkg-config \
        python3 python3-venv xz-utils
RUN curl -fsSL -o /tmp/qemu.tar.xz "https://download.qemu.org/qemu-$QEMU_VERSION.tar.xz" \
    && echo "$QEMU_SHA256  /tmp/qemu.tar.xz" | sha256sum -c \
    && tar -xJf /tmp/qemu.tar.xz -C /tmp
RUN cd "/tmp/qemu-$QEMU_VERSION" \
    && ./configure --prefix=/usr/local --target-list=x86_64-softmmu \
        --enable-kvm --enable-seccomp --enable-tools --enable-linux-aio \
        --enable-linux-io-uring --enable-zstd --disable-docs --disable-user \
        --disable-debug-info --disable-werror \
    && make -j"$(nproc)" \
    && make install DESTDIR=/out

FROM debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad AS base
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /var/lib/savestate

FROM base AS base-arm64

FROM base AS base-amd64
RUN apt-get update \
    && apt-get install -y --no-install-recommends iproute2 nftables \
        libaio1t64 libglib2.0-0t64 libpixman-1-0 libseccomp2 liburing2 libzstd1 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=qemu /out/usr/local /usr/local
RUN ! ldd /usr/local/bin/qemu-system-x86_64 /usr/local/bin/qemu-img | grep 'not found' \
    && qemu-system-x86_64 --version | grep -q '^QEMU emulator version 11\.1\.' \
    && qemu-system-x86_64 -machine help | grep -q '^pc-q35-11\.1 '

FROM base-$TARGETARCH
ARG TARGETARCH
COPY --chmod=0755 $TARGETARCH/savestate /usr/local/bin/savestate
LABEL org.opencontainers.image.source=https://github.com/samcm/savestate
VOLUME /var/lib/savestate
# tini forwards SIGTERM to savestate and reaps orphans.
ENTRYPOINT ["/usr/bin/tini", "--", "savestate"]
CMD ["--help"]
