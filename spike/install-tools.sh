#!/usr/bin/env bash
# Install the spike's tools in the QEMU container and check its QEMU.
#
# The container is the launcher image's QEMU: the `base-amd64` stage of the repository's
# Dockerfile, which builds stock QEMU 11.1.2 from the upstream release. Build it with
#
#   docker build --target base-amd64 -t savestate-qemu -f Dockerfile .
#
# so the spike measures exactly the QEMU that savestate ships.
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  ca-certificates curl python3 jq genisoimage procps util-linux >/dev/null

qemu-system-x86_64 --version | head -1
qemu-system-x86_64 --version | grep -q '^QEMU emulator version 11\.1\.' || {
  echo "this container's QEMU is not 11.1" >&2
  exit 1
}
qemu-system-x86_64 -machine help | grep -q '^pc-q35-11.1 ' || {
  echo "machine type pc-q35-11.1 missing" >&2
  exit 1
}
echo "$(qemu-system-x86_64 --version | head -1), upstream source built by the Dockerfile's qemu stage, sha256 $(sha256sum "$(command -v qemu-system-x86_64)" | cut -c1-16)" |
  tee /etc/qemu-spike-source
