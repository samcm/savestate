#!/usr/bin/env bash
# Sets up the cluster tests/e2e.rs runs against, on a host with /dev/kvm and Docker:
# a kind cluster with savestate installed from the image IMAGE (loaded in Docker), a
# plain-HTTP registry holding a base image made from ROOTFS (guest/rootfs's
# rootfs.qcow2), an S3-compatible store checkpoints are exported to, and the guest
# CPU model this host runs, picked like spike/ does.
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
# kind's network plugin does not enforce NetworkPolicies; this does.
curl -fsSL -o "$work/netpol.yaml" https://raw.githubusercontent.com/kubernetes-sigs/kube-network-policies/v1.1.2/install.yaml
echo "a8345bd40f74e812c7f2e13c40a841d4d6c7b1fc22e245750116072807a0f4ba  $work/netpol.yaml" | sha256sum -c
sed -i 's|image: registry.k8s.io/networking/kube-network-policies:.*|image: registry.k8s.io/networking/kube-network-policies:v1.1.2@sha256:c671f98e444825d57c590c92da1b5e66070917c4d3dff1f9279775e6b73254fe|' "$work/netpol.yaml"
kubectl apply -f "$work/netpol.yaml"
kubectl -n kube-system rollout status daemonset/kube-network-policies --timeout=3m
# Dynamic filesystem claims for active disks in this disposable cluster. Production
# chooses its own CSI StorageClass; the test only needs the same RWO filesystem API.
curl -fsSL -o "$work/local-path.yaml" https://raw.githubusercontent.com/rancher/local-path-provisioner/v0.0.34/deploy/local-path-storage.yaml
echo "fab8d6e8933e44f8af7398603fb63122e4ea6490f0af83549108d68647edd9e9  $work/local-path.yaml" | sha256sum -c
sed -i 's|image: rancher/local-path-provisioner:v0.0.34|image: rancher/local-path-provisioner:v0.0.34@sha256:6ff68ebe98bc623b45ad22c28be84f8a08214982710f3247d5862e9bccce73ef|' "$work/local-path.yaml"
kubectl apply -f "$work/local-path.yaml"
kubectl -n local-path-storage rollout status deployment/local-path-provisioner --timeout=3m
# Clients the sandboxes' exposed ports admit (network.exposeFrom).
kubectl create namespace clients
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

# An S3-compatible store on kind's network (MinIO, from a community build: upstream
# publishes no images any more) with a bucket for exported checkpoints; the test
# reaches it on 127.0.0.1:9000.
s3_key=savestate s3_secret=savestate-e2e-secret
docker run -d --name savestate-s3 --network kind -p 127.0.0.1:9000:9000 \
  -e MINIO_ROOT_USER=$s3_key -e MINIO_ROOT_PASSWORD=$s3_secret \
  pgsty/minio:RELEASE.2026-08-04T00-00-00Z@sha256:b6bfe7239bfc83fb90d31612d9704d86039dd714f7904b3f1ad68f211e602372 \
  server /data
for _ in $(seq 60); do curl -fs http://127.0.0.1:9000/minio/health/live && break; sleep 1; done
curl -fsS --aws-sigv4 aws:amz:us-east-1:s3 --user "$s3_key:$s3_secret" -X PUT http://127.0.0.1:9000/savestate
s3=$(docker inspect --format '{{(index .NetworkSettings.Networks "kind").IPAddress}}' savestate-s3)

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
  activeDiskStorageClass: local-path
  shapes:
    - {vcpus: 1, memory: 1Gi, disk: 4Gi, activeDiskSize: 6Gi}
    - {vcpus: 4, memory: 1Gi, disk: 4Gi, cpuRequest: 500m, activeDiskSize: 6Gi}
  nodeSelector:
    savestate.dev/node: "true"
  # The runner's disk is shared with everything else on it; how full it is says
  # nothing about savestate, so the node never turns VMs away for it here.
  minFreeDiskPercent: 0
network:
  egressAllow: [$registry/32]
  exposeFrom:
    - namespaceSelector:
        matchLabels: {kubernetes.io/metadata.name: clients}
images:
  plainHttp: ["$registry:5000"]
  prefetch: [$base]
stores:
  e2e:
    endpoint: http://$s3:9000
    bucket: savestate
    region: us-east-1
    prefix: e2e
    credentialsSecret: savestate-store-e2e
# Every exported copy not in use is dropped, as if the disk were full.
cache:
  minFreePercent: 100
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
secretGenerator:
  - name: savestate-store-e2e
    namespace: savestate-system
    options:
      disableNameSuffixHash: true
    literals:
      - accessKeyId=$s3_key
      - secretAccessKey=$s3_secret
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
SAVESTATE_E2E_NODE=$node
SAVESTATE_E2E_STORE=e2e
SAVESTATE_E2E_CLIENTS=clients
SAVESTATE_E2E_CLIENT_IMAGE=$pinned
SAVESTATE_E2E_S3_ENDPOINT=http://127.0.0.1:9000
SAVESTATE_E2E_S3_BUCKET=savestate
SAVESTATE_E2E_S3_PREFIX=e2e
SAVESTATE_E2E_S3_ACCESS_KEY_ID=$s3_key
SAVESTATE_E2E_S3_SECRET_ACCESS_KEY=$s3_secret
EOF
cat "$out"
