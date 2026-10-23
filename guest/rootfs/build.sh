#!/usr/bin/env bash
# Builds guest images from guest/rootfs/Dockerfile with docker buildx.
#
#   guest/rootfs/build.sh disk SAVESTATE_GUEST OUT_DIR
#       The generic base as OUT_DIR/rootfs.qcow2.
#   guest/rootfs/build.sh base SAVESTATE_GUEST TAG
#       The generic base as a local container image TAG, to build an installation's
#       own image FROM.
#   guest/rootfs/build.sh disk-from IMAGE OUT_DIR
#       A container image built FROM the base (local or in a registry) as
#       OUT_DIR/rootfs.qcow2.
#
# SAVESTATE_GUEST is the static x86_64 guest agent:
#   cargo build --release --target x86_64-unknown-linux-musl -p savestate-guest
# SIZE sets the root filesystem size in the qcow2 (default 2G); the guest grows it to
# its disk at boot.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
usage() {
    sed -n '3,12s/^# \{0,1\}//p' "${BASH_SOURCE[0]}" >&2
    exit 2
}
[ $# = 3 ] || usage

agent() {
    ctx="$(mktemp -d)"
    trap 'rm -rf "$ctx"' EXIT
    install -m 0755 "$1" "$ctx/savestate-guest"
    args+=(--build-context "agent=$ctx")
}

args=(--build-arg "SIZE=${SIZE:-2G}")
case "$1" in
    disk)
        agent "$2"
        args+=(--target disk --output "type=local,dest=$3")
        ;;
    base)
        agent "$2"
        args+=(--target base --tag "$3" --load)
        ;;
    disk-from)
        args+=(--build-context "rootfs=docker-image://$2" --target disk --output "type=local,dest=$3")
        ;;
    *) usage ;;
esac
docker buildx build "${args[@]}" "$here"
