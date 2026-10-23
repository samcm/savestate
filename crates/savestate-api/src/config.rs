//! The installation config: one YAML file read by the controller and the node agent.
//!
//! Everything that describes one particular installation is a field here, never a
//! constant in the code (see "Configuration boundary" in `docs/design.md`). Every
//! field has a documented default or is required; `deploy/base/config.yaml` is an
//! example with placeholder values.

use std::{collections::BTreeMap, net::IpAddr, path::PathBuf};

use ipnet::IpNet;
use k8s_openapi::{
    api::core::v1::{NodeSelector, Toleration},
    apimachinery::pkg::api::resource::Quantity,
};
use kube_quantity::ParsedQuantity;
use serde::Deserialize;

use crate::Ttl;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    /// Namespaces whose Sandboxes, Checkpoints and Transfers savestate acts on; VM
    /// pods run in their sandbox's namespace. Required, at least one. Objects in
    /// other namespaces are ignored. The controller's and node agent's RBAC is
    /// bound in each of these namespaces.
    pub namespaces: Vec<String>,

    /// Directory on every node holding images, layers and checkpoints.
    /// Default: `/var/lib/savestate`.
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,

    /// The kubelet's root directory on every VM node. The node agent serves its
    /// device plugin in `device-plugins/` and installs the VM pods' seccomp profile
    /// in `seccomp/` below it. Default: `/var/lib/kubelet`.
    #[serde(default = "default_kubelet_dir")]
    pub kubelet_dir: PathBuf,

    /// Image VM pods run, pinned by digest (`…@sha256:…`). Default: the controller's
    /// own image, by the digest its pod is running.
    #[serde(default)]
    pub launcher_image: Option<String>,

    /// VM pods.
    pub vm: Vm,

    #[serde(default)]
    pub network: Network,

    #[serde(default)]
    pub images: Images,

    /// S3-compatible object stores checkpoints can be exported to and restored
    /// from, by name (lowercase letters, digits and `-`, at most 63). Default: none.
    #[serde(default)]
    pub stores: BTreeMap<String, Store>,

    /// Store every checkpoint is exported to once `Ready`, unless its own
    /// `spec.export` names another. Default: none; only checkpoints with
    /// `spec.export` are exported.
    #[serde(default)]
    pub export: Option<String>,

    #[serde(default)]
    pub cache: Cache,

    #[serde(default)]
    pub transfers: Transfers,

    #[serde(default)]
    pub ttl: TtlLimits,
}

/// One S3-compatible object store. Requests are path-style
/// (`<endpoint>/<bucket>/<key>`).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Store {
    /// Endpoint URL, e.g. `https://s3.example.com`; `http://` only inside a network
    /// you trust. Required.
    pub endpoint: String,

    /// Bucket. Required.
    pub bucket: String,

    /// Region requests are signed for. Required (stores with no regions accept any
    /// name).
    pub region: String,

    /// Key prefix within the bucket, without leading or trailing `/`. One prefix
    /// belongs to one installation: its garbage collection deletes everything there
    /// that names none of its checkpoints. Default: empty, the bucket's root.
    #[serde(default)]
    pub prefix: String,

    /// Secret in the namespace the controller and node agents run in, holding
    /// `accessKeyId` and `secretAccessKey` (and optionally `sessionToken`). It is
    /// read on every use, so rotating it needs no restart. Required.
    pub credentials_secret: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Cache {
    /// Node-local copies of checkpoints that a store holds are a cache: while the
    /// state directory's filesystem has less than this percentage free, the node
    /// agent drops them, oldest first, except those a VM pod on the node starts or
    /// runs from. Default: 10.
    #[serde(default = "default_min_free_percent")]
    pub min_free_percent: u8,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            min_free_percent: default_min_free_percent(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Vm {
    /// Guest CPU model passed to QEMU's `-cpu`. Required. `host` gives guests the
    /// node's own CPU: the simple choice where all VM nodes are one machine type (a
    /// checkpoint restores only on nodes with an identical CPU). A named model that
    /// every VM node can run, e.g. `EPYC-Rome-v4`, is run with `enforce` and lets
    /// checkpoints move between any VM nodes. `max` is refused.
    pub cpu_model: String,

    /// Added to a sandbox's memory for its VM pod's memory request and limit, to
    /// cover QEMU itself. Default: `512Mi`.
    #[serde(default = "default_memory_overhead")]
    pub memory_overhead: Quantity,

    /// CPU a VM pod requests per vCPU, for shapes that set no `cpuRequest`: more
    /// than 0, at most 1. Below 1 the pod has no CPU limit and is Burstable (see
    /// "VM pods" in `docs/design.md`). Default: `1`, a whole CPU per vCPU
    /// (Guaranteed).
    #[serde(default = "default_cpu_request_per_vcpu")]
    pub cpu_request_per_vcpu: Quantity,

    /// Shapes a sandbox from an image may use; it must match one exactly.
    /// Default: empty, any shape.
    #[serde(default)]
    pub shapes: Vec<Shape>,

    /// Node selector for VM pods; the node-agent `DaemonSet` must cover the same
    /// nodes. Default: empty, any node.
    #[serde(default)]
    pub node_selector: BTreeMap<String, String>,

    /// Required node affinity for VM pods (`nodeSelectorTerms`, any one of which a
    /// node must match), for selections a `nodeSelector` cannot express, such as a
    /// list of hostnames. Applies on top of `nodeSelector`; the node-agent
    /// `DaemonSet` must cover the same nodes. Default: none.
    #[serde(default)]
    pub node_affinity: Option<NodeSelector>,

    /// Tolerations for VM pods. Default: none.
    #[serde(default)]
    pub tolerations: Vec<Toleration>,

    /// VM slots each node agent advertises to the kubelet (as the extended resource
    /// `savestate.dev/vm`); every VM pod takes one. Caps the VMs per node. Default:
    /// 110, the kubelet's default pod limit, so CPU and memory requests decide
    /// placement unless this is set lower.
    #[serde(default = "default_slots_per_node")]
    pub slots_per_node: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Shape {
    pub vcpus: u32,
    pub memory: Quantity,
    pub disk: Quantity,
    /// CPU the VM pod of a sandbox of this shape requests: more than 0, at most
    /// `vcpus`. Default: `vcpus` × `vm.cpuRequestPerVcpu`.
    #[serde(default)]
    pub cpu_request: Option<Quantity>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Network {
    /// Destinations in cluster, node, link-local and private ranges that guests may
    /// still reach; everything else in those ranges is dropped. Public addresses
    /// are always reachable. Default: none.
    #[serde(default)]
    pub egress_allow: Vec<IpNet>,

    /// DNS servers handed to guests (and allowed through the egress filter).
    /// Default: empty, the VM pod's own resolvers.
    #[serde(default)]
    pub dns: Vec<IpAddr>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Images {
    /// Registries (or `registry/path` prefixes) sandbox images may come from.
    /// Default: empty, any registry.
    #[serde(default)]
    pub registries: Vec<String>,

    /// Registry credentials for the node agent: a Docker `config.json` mounted
    /// into its pod. Default: none, anonymous pulls.
    #[serde(default)]
    pub auth: Option<PathBuf>,

    /// Registries (`host[:port]`) the node agent reaches over plain HTTP instead of
    /// HTTPS, e.g. a registry inside the cluster. Default: none.
    #[serde(default)]
    pub plain_http: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Transfers {
    /// Transfers each node sends or receives at a time. Default: 1.
    #[serde(default = "default_transfers_per_node")]
    pub per_node: u32,
}

impl Default for Transfers {
    fn default() -> Self {
        Self {
            per_node: default_transfers_per_node(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TtlLimits {
    #[serde(default)]
    pub sandbox: TtlLimit,
    #[serde(default)]
    pub checkpoint: TtlLimit,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TtlLimit {
    /// TTL for objects that set none. Default: none, they live until deleted.
    #[serde(default)]
    pub default: Option<Ttl>,

    /// Longest an object may live: longer TTLs, and no TTL at all, are capped to
    /// it. Default: no limit.
    #[serde(default)]
    pub max: Option<Ttl>,
}

fn default_state_dir() -> PathBuf {
    PathBuf::from("/var/lib/savestate")
}

fn default_kubelet_dir() -> PathBuf {
    PathBuf::from("/var/lib/kubelet")
}

fn default_slots_per_node() -> u32 {
    110
}

fn default_memory_overhead() -> Quantity {
    Quantity("512Mi".into())
}

fn default_cpu_request_per_vcpu() -> Quantity {
    Quantity("1".into())
}

fn default_transfers_per_node() -> u32 {
    1
}

fn default_min_free_percent() -> u8 {
    10
}

#[derive(Debug, thiserror::Error)]
#[error("installation config: {0}")]
pub struct ConfigError(String);

impl Config {
    /// Parse and check an installation config.
    pub fn from_yaml(yaml: &str) -> Result<Self, ConfigError> {
        let config: Self = serde_saphyr::from_str(yaml).map_err(|e| ConfigError(e.to_string()))?;
        config.check().map_err(ConfigError)?;
        Ok(config)
    }

    fn check(&self) -> Result<(), String> {
        if self.namespaces.is_empty() {
            return Err("namespaces: list at least one namespace for sandboxes".into());
        }
        let cpu = self.vm.cpu_model.as_str();
        match cpu.split(',').next() {
            Some("" | "max") => {
                return Err(format!(
                    "vm.cpuModel: {cpu:?}: use host, or a named CPU model every VM node can run (e.g. EPYC-Rome-v4)"
                ));
            }
            Some("host") if cpu != "host" => {
                return Err(format!("vm.cpuModel: {cpu:?}: host takes no feature flags"));
            }
            _ => {}
        }
        if let Some(image) = &self.launcher_image
            && !image.contains("@sha256:")
        {
            return Err(format!(
                "launcherImage: {image:?} must be pinned by digest (…@sha256:…)"
            ));
        }
        let cpu = |quantity: &Quantity, what: &str| {
            ParsedQuantity::try_from(quantity).map_err(|e| format!("{what}: {:?}: {e}", quantity.0))
        };
        let cpus = |n: u32| cpu(&Quantity(n.to_string()), "vcpus");
        let zero = cpus(0)?;
        let per_vcpu = cpu(&self.vm.cpu_request_per_vcpu, "vm.cpuRequestPerVcpu")?;
        if per_vcpu <= zero || per_vcpu > cpus(1)? {
            return Err(format!(
                "vm.cpuRequestPerVcpu: {:?} must be more than 0 and at most 1",
                self.vm.cpu_request_per_vcpu.0
            ));
        }
        for shape in &self.vm.shapes {
            if let Some(request) = &shape.cpu_request {
                let parsed = cpu(request, "vm.shapes cpuRequest")?;
                if parsed <= zero || parsed > cpus(shape.vcpus)? {
                    return Err(format!(
                        "vm.shapes: cpuRequest {:?} of the {}-vCPU shape must be more than 0 and at most {}",
                        request.0, shape.vcpus, shape.vcpus
                    ));
                }
            }
        }
        if self.vm.slots_per_node == 0 {
            return Err("vm.slotsPerNode: must be at least 1".into());
        }
        if self.transfers.per_node == 0 {
            return Err("transfers.perNode: must be at least 1".into());
        }
        self.check_stores()?;
        if self.cache.min_free_percent > 100 {
            return Err("cache.minFreePercent: at most 100".into());
        }
        for (kind, limit) in [
            ("sandbox", &self.ttl.sandbox),
            ("checkpoint", &self.ttl.checkpoint),
        ] {
            if let (Some(default), Some(max)) = (&limit.default, &limit.max)
                && default.duration() > max.duration()
            {
                return Err(format!("ttl.{kind}: default {default} exceeds max {max}"));
            }
        }
        Ok(())
    }

    fn check_stores(&self) -> Result<(), String> {
        for (name, store) in &self.stores {
            let label = !name.is_empty()
                && name.len() <= 63
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !name.starts_with('-')
                && !name.ends_with('-');
            if !label {
                return Err(format!(
                    "stores: {name:?}: a store name is lowercase letters, digits and -, at most 63"
                ));
            }
            if !(store.endpoint.starts_with("https://") || store.endpoint.starts_with("http://")) {
                return Err(format!(
                    "stores.{name}.endpoint: {:?} is not an http:// or https:// URL",
                    store.endpoint
                ));
            }
            for (field, value) in [
                ("bucket", &store.bucket),
                ("region", &store.region),
                ("credentialsSecret", &store.credentials_secret),
            ] {
                if value.is_empty() {
                    return Err(format!("stores.{name}.{field}: must not be empty"));
                }
            }
            if store.prefix.starts_with('/') || store.prefix.ends_with('/') {
                return Err(format!(
                    "stores.{name}.prefix: {:?} must not start or end with /",
                    store.prefix
                ));
            }
        }
        if let Some(export) = &self.export
            && !self.stores.contains_key(export)
        {
            return Err(format!("export: {export:?} is not in stores"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example an installation starts from must be accepted as shipped.
    #[test]
    fn example_config_is_valid() {
        Config::from_yaml(include_str!("../../../deploy/base/config.yaml")).unwrap();
    }
}
