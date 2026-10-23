# syntax=docker/dockerfile:1
#
# ghcr.io/samcm/savestate: every role of the `savestate` binary. linux/amd64 also
# carries QEMU for the launcher; linux/arm64 runs the controller and the CLI. Built
# from prebuilt artifacts (the build.yml outputs) in the context:
#
#   amd64/savestate  arm64/savestate
#
#   docker buildx build --platform linux/amd64,linux/arm64 -f Dockerfile <context>

FROM debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad AS base
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /var/lib/savestate

FROM base AS base-arm64

FROM base AS base-amd64
# QEMU 11.1 from trixie-backports (trixie itself has 10.0). The machine type
# (pc-q35-11.1) and every checkpoint's fingerprint depend on this build, and a
# checkpoint restores only on the image it was taken with; the check below fails the
# build if backports moves to another QEMU release.
RUN printf 'Types: deb\nURIs: http://deb.debian.org/debian\nSuites: trixie-backports\nComponents: main\nSigned-By: /usr/share/keyrings/debian-archive-keyring.gpg\n' \
        >/etc/apt/sources.list.d/backports.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends iproute2 nftables \
    && apt-get install -y --no-install-recommends -t trixie-backports qemu-system-x86 qemu-utils \
    && rm -rf /var/lib/apt/lists/* \
    && qemu-system-x86_64 --version \
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
