use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Ttl;

/// One Linux VM on one node, booted from a disk image or started from a checkpoint.
/// It runs until it is deleted (or its TTL expires).
///
/// Exactly one of `image` and `from` is set. A sandbox from an image states its shape
/// (`vcpus`, `memory`, `disk`); a sandbox from a checkpoint inherits image and shape
/// from the checkpoint and must not set them. Only `ttl` can change after creation.
#[derive(CustomResource, KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[kube(
    group = "savestate.dev",
    version = "v1alpha1",
    kind = "Sandbox",
    namespaced,
    doc = "A QEMU/KVM virtual machine booted from a disk image or started from a checkpoint",
    status = "SandboxStatus",
    shortname = "sbx",
    category = "savestate",
    derive = "PartialEq",
    selectable = ".spec.from.checkpoint",
    selectable = ".status.node",
    printcolumn(json_path = ".status.phase", name = "Phase", type_ = "string"),
    printcolumn(json_path = ".status.node", name = "Node", type_ = "string"),
    printcolumn(json_path = ".spec.from.checkpoint", name = "From", type_ = "string"),
    printcolumn(
        json_path = ".spec.vcpus",
        name = "vCPUs",
        type_ = "integer",
        priority = 1
    ),
    printcolumn(
        json_path = ".spec.memory",
        name = "Memory",
        type_ = "string",
        priority = 1
    ),
    printcolumn(
        json_path = ".spec.disk",
        name = "Disk",
        type_ = "string",
        priority = 1
    ),
    printcolumn(
        json_path = ".spec.image",
        name = "Image",
        type_ = "string",
        priority = 1
    ),
    printcolumn(
        json_path = ".metadata.creationTimestamp",
        name = "Age",
        type_ = "date"
    )
)]
#[x_kube(
    validation = Rule::new("has(self.image) != has(self.from)")
        .message("set exactly one of image and from"),
    validation = Rule::new("!has(self.image) || (has(self.vcpus) && has(self.memory) && has(self.disk))")
        .message("a sandbox from an image must set vcpus, memory and disk"),
    validation = Rule::new("!has(self.from) || !(has(self.vcpus) || has(self.memory) || has(self.disk))")
        .message("a sandbox from a checkpoint inherits vcpus, memory and disk from it; do not set them"),
    validation = Rule::new("!has(self.clock) || has(self.from)")
        .message("clock applies only to a sandbox started from a checkpoint (from)"),
    validation = Rule::new("has(self.image) == has(oldSelf.image) && has(self.clock) == has(oldSelf.clock)")
        .message("only ttl can change on a sandbox; delete it and create another"),
)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSpec {
    /// Disk image to boot, pinned by digest: `REGISTRY/REPOSITORY@sha256:DIGEST`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 1024), regex(pattern = r"^[^@\s]+@sha256:[0-9a-f]{64}$"))]
    #[x_kube(validation = Rule::new("self == oldSelf").message("image cannot change"))]
    pub image: Option<String>,

    /// Checkpoint to start from, in the sandbox's namespace. The new VM wakes
    /// exactly where the checkpoint was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[x_kube(validation = Rule::new("self == oldSelf").message("from cannot change"))]
    pub from: Option<SandboxSource>,

    /// Virtual CPUs. Required with `image`; inherited with `from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1), extend("format" = "int32"))]
    #[x_kube(validation = Rule::new("self == oldSelf").message("vcpus cannot change"))]
    pub vcpus: Option<u32>,

    /// Guest RAM, e.g. `8Gi`. Required with `image`; inherited with `from`. The VM
    /// pod requests this plus the installation's memory overhead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[x_kube(
        validation = Rule::new(POSITIVE_QUANTITY).message("memory must be a positive quantity, e.g. 8Gi"),
        validation = Rule::new("self == oldSelf").message("memory cannot change"),
    )]
    pub memory: Option<Quantity>,

    /// Disk size, e.g. `60Gi`. Required with `image`; inherited with `from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[x_kube(
        validation = Rule::new(POSITIVE_QUANTITY).message("disk must be a positive quantity, e.g. 60Gi"),
        validation = Rule::new("self == oldSelf").message("disk cannot change"),
    )]
    pub disk: Option<Quantity>,

    /// Guest clock after starting from a checkpoint: `frozen` (the default; the
    /// guest believes no time passed) or `stepped` (the wall clock is set to the
    /// host's time). Only with `from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[x_kube(validation = Rule::new("self == oldSelf").message("clock cannot change"))]
    pub clock: Option<Clock>,

    /// Delete the sandbox this long after its creation, e.g. `7d`. Can be changed
    /// to extend or shorten its life. No TTL: it runs until deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<Ttl>,
}

/// A quantity (integer or string) greater than zero.
const POSITIVE_QUANTITY: &str = "type(self) == int ? self > 0 : (isQuantity(self) && quantity(self).isGreaterThan(quantity('0')))";

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSource {
    /// Name of a checkpoint in the sandbox's namespace.
    #[schemars(length(min = 1, max = 253))]
    pub checkpoint: String,
}

#[derive(JsonSchema, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Clock {
    /// The guest's clock did not advance while it was saved.
    #[default]
    Frozen,
    /// The guest's wall clock is set to the host's current time after the restore.
    Stepped,
}

/// Written by the controller.
#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SandboxStatus {
    #[serde(default)]
    pub phase: SandboxPhase,

    /// Node the VM runs on, once placed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,

    /// Checkpoint the VM was restored from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<SandboxSource>,

    /// Why the sandbox is in its phase, mainly for `Failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SandboxPhase {
    /// Accepted; not yet placed on a node.
    #[default]
    Pending,
    /// Its checkpoint fits on no node that holds it; a `Transfer` owned by the
    /// sandbox is copying it to one that does.
    WaitingForTransfer,
    /// Booting from its image.
    Booting,
    /// Restoring from its checkpoint.
    Restoring,
    /// The VM is running.
    Running,
    /// The VM crashed or could not start. Terminal: nothing restarts it.
    Failed,
}
