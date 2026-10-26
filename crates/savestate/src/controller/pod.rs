//! The VM pod for one sandbox (see "VM pods" in `docs/design.md`).

use k8s_openapi::{
    api::core::v1::{NodeSelector, NodeSelectorRequirement, NodeSelectorTerm, Pod},
    apimachinery::pkg::api::resource::Quantity,
};
use kube::{Resource as _, ResourceExt};
use kube_quantity::ParsedQuantity;
use savestate_api::{Config, Sandbox};
use serde_json::json;

use super::Error;
use crate::{
    launcher::ports,
    node::{RESOURCE, SECCOMP_PROFILE},
};

/// Labels on every VM pod; the controller watches pods by them and the
/// `NetworkPolicy` in `deploy/base/sandboxes.yaml` selects them.
pub(super) const LABELS: &str = "app.kubernetes.io/name=savestate,app.kubernetes.io/component=vm";

pub(super) use crate::SANDBOX_UID;

/// Host directory holding one directory per sandbox for the launcher's socket
/// (`RUN_DIR/<namespace>/<sandbox>/<sandbox UID>/`), mounted at the same path,
/// `RUN_DIR`, in the VM pod.
pub(super) const RUN_DIR: &str = "/run/savestate";
pub(super) const ACTIVE_DISK_DIR: &str = "/var/lib/savestate-active";

/// What the pod runs: the launcher image, the VM's size and the CPU its pod
/// requests, and where it may run.
pub(super) struct Vm {
    pub image: String,
    pub vcpus: u32,
    pub cpu: Quantity,
    pub memory: Quantity,
    /// Node disk requested (`ephemeral-storage`) for the guest's disk layer; none
    /// for a sandbox from a checkpoint that does not record its disk.
    pub disk: Option<Quantity>,
    /// Capacity of the sandbox's active-disk PVC.
    pub active_disk: Option<Quantity>,
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
    // The guest's disk layer lives in the state directory (a hostPath), which the
    // kubelet does not count as the pod's: the request is only what the scheduler
    // and the cluster autoscaler reserve for it, so there is no limit to enforce.
    if vm.active_disk.is_none()
        && let Some(disk) = &vm.disk
    {
        requests["ephemeral-storage"] = json!(disk);
    }
    if parse(&vm.cpu, "CPU request")? >= parse(&Quantity(vm.vcpus.to_string()), "vcpus")? {
        limits["cpu"] = json!(vm.cpu);
    }
    // Root with exactly what the launcher uses before it drops to the QEMU user for
    // good: NET_ADMIN for the tap device, forwarding and nftables in its own network
    // namespace; CHOWN to hand its run directory to the QEMU user; SETUID and SETGID
    // to become it. KILL lets the image's init (root) pass SIGTERM on to the launcher,
    // which then runs as the QEMU user, so it stops QEMU and says why it stopped. The
    // seccomp profile, installed by the node agent, is the runtime's default plus
    // AF_VSOCK sockets, the guest agent's channel.
    let security = json!({
        "runAsUser": 0,
        "allowPrivilegeEscalation": false,
        "readOnlyRootFilesystem": true,
        "capabilities": {"drop": ["ALL"], "add": ["NET_ADMIN", "CHOWN", "SETUID", "SETGID", "KILL"]},
        "seccompProfile": {"type": "Localhost", "localhostProfile": SECCOMP_PROFILE},
    });
    let mut args = vec!["launcher".to_owned()];
    for net in &config.network.egress_allow {
        args.extend(["--egress-allow".into(), net.to_string()]);
    }
    for ip in &config.network.dns {
        args.extend(["--dns".into(), ip.to_string()]);
    }
    let affinity = affinity(&vm.placement, config.vm.node_affinity.as_ref());
    let state_dir = config.state_dir.display().to_string();
    let (ports_dir, ports_file) = ports::FILE.rsplit_once('/').unwrap_or_default();

    let mut mounts = vec![
        json!({"name": "state", "mountPath": state_dir}),
        json!({"name": "run", "mountPath": RUN_DIR}),
        json!({"name": "pod", "mountPath": ports_dir, "readOnly": true}),
    ];
    let mut volumes = vec![
        json!({"name": "state", "hostPath": {"path": state_dir, "type": "DirectoryOrCreate"}}),
        json!({"name": "run", "hostPath": {
            "path": format!("{RUN_DIR}/{namespace}/{}/{uid}", sandbox.name_any()),
            "type": "DirectoryOrCreate",
        }}),
        // The exposed ports, which the kubelet updates as the annotation changes.
        json!({"name": "pod", "downwardAPI": {"items": [{
            "path": ports_file,
            "fieldRef": {"fieldPath": format!("metadata.annotations['{}']", ports::ANNOTATION)},
        }]}}),
    ];
    add_active_disk_volume(sandbox, vm, &mut mounts, &mut volumes);

    // The CSI-mounted filesystem has to be writable by QEMU after the launcher
    // drops privileges. Do not apply this to the legacy host paths: the node agent
    // owns their modes and changing them on every pod start would change the
    // existing hostPath contract.
    let pod_security = active_disk_security(vm);

    let pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": sandbox.name_any(),
            "namespace": namespace,
            "labels": {
                "app.kubernetes.io/name": "savestate",
                "app.kubernetes.io/component": "vm",
                SANDBOX_UID: uid,
            },
            "annotations": {
                // Avoid voluntary eviction: the disk survives replacement, but RAM
                // does not unless the user took a checkpoint.
                "cluster-autoscaler.kubernetes.io/safe-to-evict": "false",
                // Changed by the controller as `spec.ports` changes.
                ports::ANNOTATION: ports::format(&sandbox.spec.ports),
            },
            "ownerReferences": [owner],
        },
        "spec": {
            // The launcher has no Kubernetes credentials.
            "automountServiceAccountToken": false,
            "enableServiceLinks": false,
            // A launcher/QEMU exit is terminal. The controller only replaces a pod
            // removed by infrastructure when its active disk is PVC-backed.
            "restartPolicy": "Never",
            "securityContext": pod_security,
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
                "volumeMounts": mounts,
            }],
            "volumes": volumes,
        },
    });
    serde_json::from_value(pod).map_err(|error| Error::PodSpec(error.to_string()))
}

fn add_active_disk_volume(
    sandbox: &Sandbox,
    vm: &Vm,
    mounts: &mut Vec<serde_json::Value>,
    volumes: &mut Vec<serde_json::Value>,
) {
    if vm.active_disk.is_some() {
        mounts.push(json!({
            "name": "active-disk", "mountPath": ACTIVE_DISK_DIR,
        }));
        volumes.push(json!({
            "name": "active-disk", "persistentVolumeClaim": {"claimName": sandbox.name_any()},
        }));
    }
}

fn active_disk_security(vm: &Vm) -> Option<serde_json::Value> {
    vm.active_disk.as_ref().map(|_| {
        json!({
            "fsGroup": crate::store::QEMU_GID,
            "fsGroupChangePolicy": "OnRootMismatch",
        })
    })
}

/// Node affinity for `placement`, within the installation's `vm.nodeAffinity`, and the
/// preference for nodes running VMs already.
/// Configured terms are alternatives, so the placement's requirement is added to
/// every term rather than as a term of its own.
fn affinity(placement: &Placement, configured: Option<&NodeSelector>) -> serde_json::Value {
    let requirement = |key: &str, values: &[String]| NodeSelectorRequirement {
        key: key.into(),
        operator: "In".into(),
        values: Some(values.to_vec()),
    };
    let (fields, expressions, prefer) = match placement {
        Placement::Any => (None, None, &[][..]),
        Placement::Nodes(nodes) => (Some(requirement("metadata.name", nodes)), None, &[][..]),
        Placement::Cpu { label, prefer } => (
            None,
            Some(requirement(crate::CPU_LABEL, std::slice::from_ref(label))),
            prefer.as_slice(),
        ),
    };
    let mut required = configured.cloned();
    if fields.is_some() || expressions.is_some() {
        let required = required.get_or_insert_with(|| NodeSelector {
            node_selector_terms: vec![NodeSelectorTerm::default()],
        });
        for term in &mut required.node_selector_terms {
            if let Some(fields) = &fields {
                term.match_fields
                    .get_or_insert_with(Vec::new)
                    .push(fields.clone());
            }
            if let Some(expressions) = &expressions {
                term.match_expressions
                    .get_or_insert_with(Vec::new)
                    .push(expressions.clone());
            }
        }
    }
    let mut node = json!({});
    if let Some(required) = required {
        node["requiredDuringSchedulingIgnoredDuringExecution"] = json!(required);
    }
    if !prefer.is_empty() {
        node["preferredDuringSchedulingIgnoredDuringExecution"] = json!([{
            "weight": 100,
            "preference": {"matchFields": [
                {"key": "metadata.name", "operator": "In", "values": prefer}
            ]},
        }]);
    }
    // Pack VMs onto nodes already running VMs (in any namespace), so the cluster
    // autoscaler can empty and remove the others. A preference only: it never keeps
    // a pod from a node with room, and the nodes holding the checkpoint come first.
    let mut affinity = json!({"podAffinity": {"preferredDuringSchedulingIgnoredDuringExecution": [{
        "weight": 50,
        "podAffinityTerm": {
            "labelSelector": {"matchLabels": {
                "app.kubernetes.io/name": "savestate",
                "app.kubernetes.io/component": "vm",
            }},
            "namespaceSelector": {},
            "topologyKey": "kubernetes.io/hostname",
        },
    }]}});
    if node != json!({}) {
        affinity["nodeAffinity"] = node;
    }
    affinity
}

pub(super) fn parse(quantity: &Quantity, what: &str) -> Result<ParsedQuantity, Error> {
    ParsedQuantity::try_from(quantity)
        .map_err(|error| Error::PodSpec(format!("{what} {:?}: {error}", quantity.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> Sandbox {
        serde_json::from_value(json!({
            "apiVersion": "savestate.dev/v1alpha1",
            "kind": "Sandbox",
            "metadata": {"name": "web", "namespace": "sandboxes", "uid": "sandbox-uid"},
            "spec": {
                "image": "ghcr.io/example/base@sha256:00",
                "vcpus": 2,
                "memory": "3Gi",
                "disk": "20Gi",
            },
        }))
        .unwrap()
    }

    fn config(active: bool) -> Config {
        let active = if active {
            "  activeDiskStorageClass: block\n  shapes:\n    - {vcpus: 2, memory: 3Gi, disk: 20Gi, activeDiskSize: 24Gi}\n"
        } else {
            ""
        };
        Config::from_yaml(&format!(
            "namespaces: [sandboxes]\nvm:\n  cpuModel: host\n{active}"
        ))
        .unwrap()
    }

    fn vm(active: bool) -> Vm {
        Vm {
            image: "ghcr.io/example/savestate@sha256:00".into(),
            vcpus: 2,
            cpu: Quantity("500m".into()),
            memory: Quantity("3Gi".into()),
            disk: Some(Quantity("20Gi".into())),
            active_disk: active.then(|| Quantity("24Gi".into())),
            placement: Placement::Any,
        }
    }

    /// PVC mode mounts the sandbox's claim for QEMU and reserves no node-local
    /// ephemeral storage. Legacy mode keeps its hostPath and resource contract.
    #[test]
    fn active_disk_volume_is_opt_in() {
        let durable = build(&sandbox(), &vm(true), &config(true)).unwrap();
        let spec = durable.spec.unwrap();
        let security = spec.security_context.unwrap();
        assert_eq!(security.fs_group, Some(i64::from(crate::store::QEMU_GID)));
        assert_eq!(
            security.fs_group_change_policy.as_deref(),
            Some("OnRootMismatch")
        );
        let volumes = serde_json::to_value(spec.volumes).unwrap();
        assert!(volumes.as_array().unwrap().iter().any(|volume| {
            volume["name"] == "active-disk" && volume["persistentVolumeClaim"]["claimName"] == "web"
        }));
        let container = &spec.containers[0];
        let mounts = serde_json::to_value(&container.volume_mounts).unwrap();
        assert!(mounts.as_array().unwrap().iter().any(|mount| {
            mount["name"] == "active-disk" && mount["mountPath"] == ACTIVE_DISK_DIR
        }));
        let requests = container
            .resources
            .as_ref()
            .and_then(|resources| resources.requests.as_ref())
            .unwrap();
        assert!(!requests.contains_key("ephemeral-storage"));

        let legacy = build(&sandbox(), &vm(false), &config(false)).unwrap();
        let spec = legacy.spec.unwrap();
        assert!(spec.security_context.is_none());
        let volumes = serde_json::to_value(spec.volumes).unwrap();
        assert!(!volumes.to_string().contains("active-disk"));
        let requests = spec.containers[0]
            .resources
            .as_ref()
            .and_then(|resources| resources.requests.as_ref())
            .unwrap();
        assert_eq!(requests["ephemeral-storage"].0, "20Gi");
    }

    /// A sandbox from a checkpoint must land on a node that holds the checkpoint
    /// and that the installation allows, under every alternative term: a pin added
    /// as a term of its own would let the pod run on any allowed node instead.
    #[test]
    fn placement_narrows_every_configured_term() {
        let term = |host: &str| json!({"matchExpressions": [{"key": "kubernetes.io/hostname", "operator": "In", "values": [host]}]});
        let configured: NodeSelector =
            serde_json::from_value(json!({"nodeSelectorTerms": [term("node-a"), term("node-b")]}))
                .unwrap();
        let pin = json!([{"key": "metadata.name", "operator": "In", "values": ["node-b"]}]);

        let affinity = affinity(&Placement::Nodes(vec!["node-b".into()]), Some(&configured));

        let terms = &affinity["nodeAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]["nodeSelectorTerms"];
        let mut want_a = term("node-a");
        want_a["matchFields"] = pin.clone();
        let mut want_b = term("node-b");
        want_b["matchFields"] = pin;
        assert_eq!(terms, &json!([want_a, want_b]));
    }
}
