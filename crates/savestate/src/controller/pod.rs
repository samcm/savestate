//! The VM pod for one sandbox (see "VM pods" in `docs/design.md`).

use k8s_openapi::{api::core::v1::Pod, apimachinery::pkg::api::resource::Quantity};
use kube::{Resource, ResourceExt};
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

/// What the pod runs: the launcher image and the VM's size, and for a sandbox from
/// a checkpoint the nodes it may run on.
pub(super) struct Vm {
    pub image: String,
    pub vcpus: u32,
    pub memory: Quantity,
    pub nodes: Option<Vec<String>>,
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
    // Guaranteed QoS: requests equal limits for everything. One VM slot from the node
    // agent's device plugin brings /dev/kvm, /dev/net/tun and /dev/vhost-vsock.
    let resources = json!({
        "cpu": vm.vcpus.to_string(),
        "memory": Quantity::from(memory),
        RESOURCE: "1",
    });
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
    let affinity = vm.nodes.as_ref().map(|nodes| {
        json!({"nodeAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": {
            "nodeSelectorTerms": [{"matchFields": [
                {"key": "metadata.name", "operator": "In", "values": nodes}
            ]}]
        }}})
    });
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
                "resources": {"requests": resources, "limits": resources},
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

/// Whether `pod` belongs to this `sandbox` (and not to an earlier sandbox of the
/// same name).
pub(super) fn owned_by(pod: &Pod, sandbox: &Sandbox) -> bool {
    let uid = sandbox.meta().uid.as_deref();
    pod.owner_references()
        .iter()
        .any(|owner| Some(owner.uid.as_str()) == uid)
}

pub(super) fn parse(quantity: &Quantity, what: &str) -> Result<ParsedQuantity, Error> {
    ParsedQuantity::try_from(quantity)
        .map_err(|error| Error::PodSpec(format!("{what} {:?}: {error}", quantity.0)))
}
