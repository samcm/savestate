//! The installation config: one YAML file read by the controller and the node agent.
//!
//! Everything that describes one particular installation is a field here, never a
//! constant in the code (see "Configuration boundary" in `docs/design.md`). Every
//! field has a documented default or is required; `deploy/base/config.yaml` is an
//! example with placeholder values.

use std::{collections::BTreeMap, net::IpAddr, path::PathBuf};

use ipnet::IpNet;
use k8s_openapi::{api::core::v1::Toleration, apimachinery::pkg::api::resource::Quantity};
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

    #[serde(default)]
    pub transfers: Transfers,

    #[serde(default)]
    pub ttl: TtlLimits,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Vm {
    /// Guest CPU model passed to QEMU's `-cpu`. Required. `host` gives guests the
    /// node's own CPU: the simple choice when checkpoints stay on the node that took
    /// them (a restore is refused on a node whose CPU differs). A named model that
    /// every VM node can run, e.g. `EPYC-Rome-v4`, is run with `enforce` and is what
    /// lets checkpoints move between nodes later. `max` is refused.
    pub cpu_model: String,

    /// Added to a sandbox's memory for its VM pod's memory request and limit, to
    /// cover QEMU itself. Default: `512Mi`.
    #[serde(default = "default_memory_overhead")]
    pub memory_overhead: Quantity,

    /// Shapes a sandbox from an image may use; it must match one exactly.
    /// Default: empty, any shape.
    #[serde(default)]
    pub shapes: Vec<Shape>,

    /// Node selector for VM pods; the node-agent `DaemonSet` must cover the same
    /// nodes. Default: empty, any node.
    #[serde(default)]
    pub node_selector: BTreeMap<String, String>,

    /// Tolerations for VM pods. Default: none.
    #[serde(default)]
    pub tolerations: Vec<Toleration>,

    /// Extended resources each VM pod requests to get `/dev/kvm` and
    /// `/dev/net/tun` from a device plugin, e.g. `{devices.example.com/kvm: 1}`.
    /// Default: empty; the launcher container is then privileged to reach them.
    #[serde(default)]
    pub devices: BTreeMap<String, Quantity>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Shape {
    pub vcpus: u32,
    pub memory: Quantity,
    pub disk: Quantity,
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

fn default_memory_overhead() -> Quantity {
    Quantity("512Mi".into())
}

fn default_transfers_per_node() -> u32 {
    1
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
        if self.transfers.per_node == 0 {
            return Err("transfers.perNode: must be at least 1".into());
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
