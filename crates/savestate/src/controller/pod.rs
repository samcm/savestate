//! The VM pod for one sandbox (see "VM pods" in `docs/design.md`).

use k8s_openapi::{api::core::v1::Pod, apimachinery::pkg::api::resource::Quantity};
use kube::{Resource, ResourceExt};
use kube_quantity::ParsedQuantity;
use savestate_api::{Config, Sandbox};
use serde_json::json;

use super::Error;

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
    // Guaranteed QoS: requests equal limits for everything.
    let mut resources = json!({"cpu": vm.vcpus.to_string(), "memory": Quantity::from(memory)});
    for (name, quantity) in &config.vm.devices {
        resources[name] = json!(quantity);
    }
    // Without a device plugin for /dev/kvm and /dev/net/tun the container reaches
    // them as privileged; with one it needs only NET_ADMIN for its tap device and
    // nftables, inside its own network namespace.
    let security = if config.vm.devices.is_empty() {
        json!({"privileged": true})
    } else {
        json!({"capabilities": {"add": ["NET_ADMIN"]}})
    };
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
                "args": ["launcher"],
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
