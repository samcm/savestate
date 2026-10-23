#!/usr/bin/env bash
# Sets up the cluster tests/e2e.rs runs against, on a host with /dev/kvm and Docker:
# a kind cluster with savestate installed from the image IMAGE (loaded in Docker), a
# plain-HTTP registry holding a base image made from ROOTFS (guest/rootfs's
# rootfs.qcow2), and the guest CPU model this host runs, picked like spike/ does.
# VM pods get their devices from the node agent's device plugin. Writes the test's
# environment to OUT (KEY=VALUE lines).
#
#   crates/savestate/tests/e2e/setup.sh IMAGE ROOTFS OUT
#
# KIND_NODE_IMAGE picks the Kubernetes version (default: kind's own).
set -euo pipefail

image=${1:?savestate image in Docker}
rootfs=${2:?rootfs.qcow2}
out=${3:?environment file to write}
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)
work=$(mktemp -d)
ns=sandboxes

# The guest CPU model: the first named model this host runs under enforce.
cpu=$(docker run --rm --device /dev/kvm -v "$repo/spike:/spike:ro" -e WORK=/tmp -e OUT=/tmp \
  --entrypoint bash "$image" /spike/select-cpu.sh | tail -n1)
echo "guest CPU model: $cpu"

kind create cluster --name savestate ${KIND_NODE_IMAGE:+--image "$KIND_NODE_IMAGE"} --wait 2m
node=savestate-control-plane
kubectl label node "$node" savestate.dev/node=true
# Room for three 1-vCPU VMs on a 4-CPU runner: cluster DNS is not needed.
kubectl -n kube-system scale deployment coredns --replicas=0

# The base image: a container image holding /rootfs.qcow2, in a registry on kind's
# network, reached by IP over plain HTTP.
docker run -d --name savestate-registry --network kind -p 127.0.0.1:5001:5000 \
  registry:3@sha256:01cd312e67026f06f5d89c3e133a4a9adc7cb9eeb9b1194e0b2954dfd770efd2
cp "$rootfs" "$work/rootfs.qcow2"
printf 'FROM scratch\nCOPY rootfs.qcow2 /rootfs.qcow2\n' >"$work/Dockerfile"
docker build -q -t localhost:5001/savestate-test/base:e2e "$work"
docker push -q localhost:5001/savestate-test/base:e2e
digest=$(docker inspect --format '{{index .RepoDigests 0}}' localhost:5001/savestate-test/base:e2e | cut -d@ -f2)
registry=$(docker inspect --format '{{(index .NetworkSettings.Networks "kind").IPAddress}}' savestate-registry)
base="$registry:5000/savestate-test/base@$digest"

# savestate's image on the node, named by its digest like a pulled one.
docker tag "$image" savestate-test/savestate:e2e
kind load docker-image --name savestate savestate-test/savestate:e2e
ref=docker.io/savestate-test/savestate:e2e
image_digest=$(docker exec "$node" ctr -n k8s.io images ls "name==$ref" | awk 'NR == 2 {print $3}')
docker exec "$node" ctr -n k8s.io images tag "$ref" "docker.io/savestate-test/savestate@$image_digest"
pinned="docker.io/savestate-test/savestate@$image_digest"

# Install from an overlay, as an operator does (deploy/README.md).
overlay=$repo/target/e2e-overlay
mkdir -p "$overlay"
cat >"$overlay/config.yaml" <<EOF
namespaces: [$ns]
launcherImage: $pinned
vm:
  cpuModel: "$cpu"
  nodeSelector:
    savestate.dev/node: "true"
network:
  egressAllow: [$registry/32]
images:
  plainHttp: ["$registry:5000"]
EOF
cat >"$overlay/kustomization.yaml" <<EOF
resources:
  - ../../deploy/base
images:
  - name: ghcr.io/samcm/savestate
    newName: docker.io/savestate-test/savestate
    digest: $image_digest
configMapGenerator:
  - name: savestate-config
    namespace: savestate-system
    behavior: replace
    files:
      - config.yaml
EOF
kubectl apply -k "$overlay"
kubectl -n savestate-system rollout status deployment/savestate-controller --timeout=3m
kubectl -n savestate-system rollout status daemonset/savestate-node --timeout=3m
# The node agent's device plugin advertises the VM slots (vm.slotsPerNode).
for i in $(seq 60); do
  [ "$(kubectl get node "$node" -o jsonpath='{.status.allocatable.savestate\.dev/vm}')" = 110 ] && break
  [ "$i" = 60 ] && { echo "the node agent advertises no VM slots" >&2; exit 1; }
  sleep 2
done
kubectl get node "$node" -o jsonpath='{.status.allocatable}'
echo

api=$(kubectl get service kubernetes -o jsonpath='{.spec.clusterIP}')
cat >"$out" <<EOF
SAVESTATE_E2E_IMAGE=$base
SAVESTATE_NAMESPACE=$ns
SAVESTATE_E2E_ALLOWED=$registry:5000
SAVESTATE_E2E_BLOCKED=$api:443
SAVESTATE_E2E_CPU_MODEL=$cpu
EOF
cat "$out"
