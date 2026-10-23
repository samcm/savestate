//! The VM pod for one sandbox (see "VM pods" in `docs/design.md`).

use k8s_openapi::{api::core::v1::Pod, apimachinery::pkg::api::resource::Quantity};
use kube::{Resource as _, ResourceExt};
use kube_quantity::ParsedQuantity;
use savestate_api::{Config, Sandbox};
use serde_json::json;

use super::Error;
use crate::node::{RESOURCE, SECCOMP_PROFILE};

/// Labels on every VM pod; the controller watches pods by them and the
/// `NetworkPolicy` in `deploy/base/sandboxes.yaml` selects them.
pub(super) const LABELS: &str = "app.kubernetes.io/name=savestate,app.kubernetes.io/component=vm";

/// Host directory holding one directory per sandbox for the launcher's socket
/// (`RUN_DIR/<namespace>/<sandbox>/<sandbox UID>/`), mounted at the same path,
/// `RUN_DIR`, in the VM pod.
pub(super) const RUN_DIR: &str = "/run/savestate";

/// What the pod runs: the launcher image, the VM's size and the CPU its pod
/// requests, and where it may run.
pub(super) struct Vm {
    pub image: String,
    pub vcpus: u32,
    pub cpu: Quantity,
    pub memory: Quantity,
    pub placement: Placement,
}

/// Nodes a VM pod may run on, beyond `vm.nodeSelector`.
pub(super) enum Placement {
    /// Any (a sandbox from an image).
    Any,
    /// Only these: the nodes holding a checkpoint no store has.
    Nodes(Vec<String>),
    /// Nodes whose guests get the checkpoint's CPU (`CPU_LABEL` equal to this),
    /// preferring these, which hold it: a checkpoint in a store.
    Cpu { label: String, prefer: Vec<String> },
}

/// The VM pod for `sandbox`, named after it and owned by it.
pub(super) fn build(sandbox: &Sandbox, vm: &Vm, config: &Config) -> Result<Pod, Error> {
    let namespace = sandbox.namespace().unwrap_or_default();
    let uid = sandbox.uid().unwrap_or_default();
    let owner = sandbox
        .controller_owner_ref(&())
        .ok_or_else(|| Error::PodSpec("the sandbox has no UID yet".into()))?;

    let memory =
        parse(&vm.memory, "memory")? + parse(&config.vm.memory_overhead, "vm.memoryOverhead")?;
    // Memory is never overcommitted: its request is its limit. One VM slot from the
    // node agent's device plugin brings /dev/kvm, /dev/net/tun and /dev/vhost-vsock.
    // A pod requesting a whole CPU per vCPU is limited to that too (Guaranteed QoS);
    // one requesting less has no CPU limit (Burstable), so QEMU's own threads are
    // never throttled while its vCPUs run.
    let mut limits = json!({"memory": Quantity::from(memory), RESOURCE: "1"});
    let mut requests = limits.clone();
    requests["cpu"] = json!(vm.cpu);
    if parse(&vm.cpu, "CPU request")? >= parse(&Quantity(vm.vcpus.to_string()), "vcpus")? {
        limits["cpu"] = json!(vm.cpu);
    }
    // Root with exactly what the launcher uses before it drops to the QEMU user for
    // good: NET_ADMIN for the tap device, forwarding and nftables in its own network
    // namespace; CHOWN to hand its run directory to the QEMU user; SETUID and SETGID
    // to become it. The seccomp profile, installed by the node agent, is the runtime's
    // default plus AF_VSOCK sockets, the guest agent's channel.
    let security = json!({
        "runAsUser": 0,
        "allowPrivilegeEscalation": false,
        "readOnlyRootFilesystem": true,
        "capabilities": {"drop": ["ALL"], "add": ["NET_ADMIN", "CHOWN", "SETUID", "SETGID"]},
        "seccompProfile": {"type": "Localhost", "localhostProfile": SECCOMP_PROFILE},
    });
    let mut args = vec!["launcher".to_owned()];
    for net in &config.network.egress_allow {
        args.extend(["--egress-allow".into(), net.to_string()]);
    }
    for ip in &config.network.dns {
        args.extend(["--dns".into(), ip.to_string()]);
    }
    let affinity = affinity(&vm.placement);
    let state_dir = config.state_dir.display().to_string();

    let pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": sandbox.name_any(),
            "namespace": namespace,
            "labels": {
                "app.kubernetes.io/name": "savestate",
                "app.kubernetes.io/component": "vm",
            },
            // Evicting a VM pod destroys its VM.
            "annotations": {"cluster-autoscaler.kubernetes.io/safe-to-evict": "false"},
            "ownerReferences": [owner],
        },
        "spec": {
            // The launcher has no Kubernetes credentials.
            "automountServiceAccountToken": false,
            "enableServiceLinks": false,
            // A VM that stops is never restarted: its sandbox becomes Failed.
            "restartPolicy": "Never",
            "nodeSelector": config.vm.node_selector,
            "tolerations": config.vm.tolerations,
            "affinity": affinity,
            "containers": [{
                "name": "launcher",
                "image": vm.image,
                "args": args,
                // The node's addresses, which the guest may not reach.
                "env": [{"name": "SAVESTATE_HOST_IPS", "valueFrom": {"fieldRef": {"fieldPath": "status.hostIPs"}}}],
                // The launcher writes why the VM stopped to its termination log.
                "terminationMessagePolicy": "FallbackToLogsOnError",
                "resources": {"requests": requests, "limits": limits},
                "securityContext": security,
                // The launcher creates `ready` once the VM runs; the pod's Ready
                // condition is how the controller learns it.
                "readinessProbe": {
                    "exec": {"command": ["test", "-e", format!("{RUN_DIR}/ready")]},
                    "periodSeconds": 2,
                },
                "volumeMounts": [
                    {"name": "state", "mountPath": state_dir},
                    {"name": "run", "mountPath": RUN_DIR},
                ],
            }],
            "volumes": [
                {"name": "state", "hostPath": {"path": state_dir, "type": "DirectoryOrCreate"}},
                {"name": "run", "hostPath": {
                    "path": format!("{RUN_DIR}/{namespace}/{}/{uid}", sandbox.name_any()),
                    "type": "DirectoryOrCreate",
                }},
            ],
        },
    });
    serde_json::from_value(pod).map_err(|error| Error::PodSpec(error.to_string()))
}

/// Node affinity for `placement`.
fn affinity(placement: &Placement) -> Option<serde_json::Value> {
    match placement {
        Placement::Any => None,
        Placement::Nodes(nodes) => Some(json!({"nodeAffinity": {
            "requiredDuringSchedulingIgnoredDuringExecution": {"nodeSelectorTerms": [{"matchFields": [
                {"key": "metadata.name", "operator": "In", "values": nodes}
            ]}]},
        }})),
        Placement::Cpu { label, prefer } => {
            let mut affinity = json!({"nodeAffinity": {
                "requiredDuringSchedulingIgnoredDuringExecution": {"nodeSelectorTerms": [{"matchExpressions": [
                    {"key": crate::CPU_LABEL, "operator": "In", "values": [label]}
                ]}]},
            }});
            if !prefer.is_empty() {
                affinity["nodeAffinity"]["preferredDuringSchedulingIgnoredDuringExecution"] = json!([{
                    "weight": 100,
                    "preference": {"matchFields": [
                        {"key": "metadata.name", "operator": "In", "values": prefer}
                    ]},
                }]);
            }
            Some(affinity)
        }
    }
}

pub(super) fn parse(quantity: &Quantity, what: &str) -> Result<ParsedQuantity, Error> {
    ParsedQuantity::try_from(quantity)
        .map_err(|error| Error::PodSpec(format!("{what} {:?}: {error}", quantity.0)))
}
